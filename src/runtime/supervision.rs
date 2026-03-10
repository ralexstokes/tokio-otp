use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant as StdInstant},
};

use slab::Slab;
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::{Id, JoinError, JoinSet},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::{
    error::{ControlError, SupervisorError, SupervisorExit},
    event::{ExitStatusView, SupervisorEvent},
    handle::{NestedControlRegistry, SupervisorCommand},
    observability::{SupervisorObservability, format_child_path},
    restart::{Restart, RestartIntensity},
    snapshot::{
        ChildMembershipView, ChildSnapshot, ChildStateView, NestedSnapshotNotification,
        NestedSnapshotState, SupervisorSnapshot, SupervisorStateView,
    },
    strategy::Strategy,
    supervisor::SupervisorConfig,
};

use super::{
    child_runtime::{ChildRuntime, RuntimeChildState},
    exit::ExitStatus,
};

type ChildKey = usize;

pub(crate) struct ChildEnvelope {
    pub(crate) key: ChildKey,
    pub(crate) instance: u64,
    pub(crate) generation: u64,
    pub(crate) result: crate::child::ChildResult,
}

#[derive(Clone, Copy)]
pub(crate) struct TaskMeta {
    pub(crate) key: ChildKey,
    pub(crate) instance: u64,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SupervisorState {
    Running,
    Stopping,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MembershipState {
    Active,
    Removing,
    Removed,
}

pub(crate) struct ChildEntry {
    pub(crate) id: String,
    pub(crate) formatted_path: String,
    pub(crate) instance: u64,
    pub(crate) runtime: ChildRuntime,
    terminal_status: Option<TerminalStatus>,
    last_exit: Option<ExitStatusView>,
    pub(crate) nested_snapshot: Option<SupervisorSnapshot>,
    pub(crate) nested_snapshot_state: Option<NestedSnapshotState>,
    pub(crate) membership: MembershipState,
}

impl ChildEntry {
    fn new(
        id: String,
        formatted_path: String,
        spec: Arc<crate::child::ChildSpecInner>,
        default_restart_intensity: RestartIntensity,
        instance: u64,
    ) -> Self {
        Self {
            id,
            formatted_path,
            instance,
            runtime: ChildRuntime::new(spec, default_restart_intensity),
            terminal_status: None,
            last_exit: None,
            nested_snapshot: None,
            nested_snapshot_state: None,
            membership: MembershipState::Active,
        }
    }
}

/// Read-only configuration and identity, fixed at construction time.
pub(crate) struct RuntimeMeta {
    pub(crate) strategy: Strategy,
    pub(crate) default_restart_intensity: RestartIntensity,
    pub(crate) registry: Arc<NestedControlRegistry>,
    pub(crate) path_prefix: Vec<String>,
    pub(crate) observability: SupervisorObservability,
}

pub(crate) struct SupervisorRuntime {
    pub(crate) meta: RuntimeMeta,
    pub(crate) state: SupervisorState,
    pub(crate) group_token: CancellationToken,
    pub(crate) join_set: JoinSet<ChildEnvelope>,
    pub(crate) children: Slab<ChildEntry>,
    pub(crate) children_by_id: HashMap<String, ChildKey>,
    pub(crate) child_order: Vec<ChildKey>,
    pub(crate) running_children: usize,
    pub(crate) live_tasks: usize,
    pub(crate) next_child_instance: u64,
    pub(crate) events: broadcast::Sender<SupervisorEvent>,
    pub(crate) snapshots: watch::Sender<SupervisorSnapshot>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    pub(crate) command_rx: mpsc::Receiver<SupervisorCommand>,
    pub(crate) nested_snapshot_tx: mpsc::Sender<NestedSnapshotNotification>,
    pub(crate) nested_snapshot_rx: mpsc::Receiver<NestedSnapshotNotification>,
    pub(crate) commands_open: bool,
    pub(crate) task_map: HashMap<Id, TaskMeta>,
    pub(crate) pending_exit: Option<Result<SupervisorExit, SupervisorError>>,
    pub(crate) last_exit: Option<SupervisorExit>,
}

impl SupervisorRuntime {
    pub(crate) fn new(
        config: SupervisorConfig,
        shutdown_rx: watch::Receiver<bool>,
        events: broadcast::Sender<SupervisorEvent>,
        snapshots: watch::Sender<SupervisorSnapshot>,
        command_rx: mpsc::Receiver<SupervisorCommand>,
        registry: Arc<NestedControlRegistry>,
        path_prefix: Vec<String>,
    ) -> Self {
        let default_restart_intensity = config.restart_intensity;
        let observability = SupervisorObservability::new(path_prefix.clone(), config.strategy);
        let mut children = Slab::with_capacity(config.children.len());
        let mut children_by_id = HashMap::with_capacity(config.children.len());
        let mut child_order = Vec::with_capacity(config.children.len());
        let mut next_child_instance = 0u64;

        for spec in config.children {
            let id = spec.id.clone();
            let formatted_path = format_child_path(&path_prefix, &id);
            let key = children.insert(ChildEntry::new(
                id.clone(),
                formatted_path,
                spec,
                default_restart_intensity,
                next_child_instance,
            ));
            next_child_instance = next_child_instance.saturating_add(1);
            children_by_id.insert(id.clone(), key);
            child_order.push(key);
        }
        let nested_snapshot_capacity = config
            .control_channel_capacity
            .max(config.event_channel_capacity);
        let (nested_snapshot_tx, nested_snapshot_rx) = mpsc::channel(nested_snapshot_capacity);

        Self {
            meta: RuntimeMeta {
                strategy: config.strategy,
                default_restart_intensity,
                registry,
                path_prefix,
                observability,
            },
            state: SupervisorState::Running,
            group_token: CancellationToken::new(),
            join_set: JoinSet::new(),
            children,
            children_by_id,
            child_order,
            running_children: 0,
            live_tasks: 0,
            next_child_instance,
            events,
            snapshots,
            shutdown_rx,
            command_rx,
            nested_snapshot_tx,
            nested_snapshot_rx,
            commands_open: true,
            task_map: HashMap::new(),
            pending_exit: None,
            last_exit: None,
        }
    }

    pub(crate) async fn run(&mut self) -> Result<SupervisorExit, SupervisorError> {
        self.send_event(SupervisorEvent::SupervisorStarted);
        let initial_children = self.child_order.clone();
        for key in initial_children {
            self.spawn_child(key)?;
        }

        loop {
            if let Some(result) = self.pending_exit.take() {
                return result;
            }

            if self.live_tasks == 0 && self.no_running_children() {
                return self.finish_natural_exit();
            }

            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    if self.shutdown_requested(changed) {
                        return self.shutdown_all().await;
                    }
                }
                command = self.command_rx.recv(), if self.commands_open => {
                    match command {
                        Some(command) => self.handle_command(command).await,
                        None => self.commands_open = false,
                    }
                    if let Some(result) = self.pending_exit.take() {
                        return result;
                    }
                }
                update = self.nested_snapshot_rx.recv() => {
                    if let Some(update) = update {
                        self.handle_nested_snapshot(update);
                    }
                }
                maybe = self.join_set.join_next_with_id() => {
                    let Some(joined) = maybe else {
                        return self.finish_natural_exit();
                    };
                    self.handle_joined_child(joined).await?;
                    if let Some(result) = self.pending_exit.take() {
                        return result;
                    }
                }
            }
        }
    }

    fn finish_natural_exit(&mut self) -> Result<SupervisorExit, SupervisorError> {
        let exit = if self
            .child_order
            .iter()
            .filter_map(|&key| self.children.get(key))
            .any(|entry| {
                entry
                    .terminal_status
                    .as_ref()
                    .is_some_and(TerminalStatus::is_failure)
            }) {
            SupervisorExit::Failed
        } else {
            SupervisorExit::Completed
        };
        self.finish_with_exit(exit);
        Ok(exit)
    }

    async fn handle_command(&mut self, command: SupervisorCommand) {
        match command {
            SupervisorCommand::AddChild { child, reply } => {
                let _ = reply.send(self.add_child(child));
            }
            SupervisorCommand::RemoveChild { id, reply } => {
                let _ = reply.send(self.remove_child(id).await);
            }
        }
    }

    fn add_child(&mut self, child: crate::child::ChildSpec) -> Result<(), ControlError> {
        if self.state == SupervisorState::Stopping {
            return Err(ControlError::SupervisorStopping);
        }

        if child.id().is_empty() {
            return Err(ControlError::InvalidConfig("child id must not be empty"));
        }

        if let Some(restart_intensity) = child.restart_intensity_override() {
            restart_intensity
                .validate()
                .map_err(|err| map_build_error_to_control(child.id(), err))?;
        }

        let id = child.id().to_owned();
        if self.children_by_id.contains_key(&id) {
            return Err(ControlError::DuplicateChildId(id));
        }

        let formatted_path = format_child_path(&self.meta.path_prefix, &id);
        let key = self.children.insert(ChildEntry::new(
            id.clone(),
            formatted_path,
            Arc::clone(&child.inner),
            self.meta.default_restart_intensity,
            self.next_child_instance,
        ));
        self.next_child_instance = self.next_child_instance.saturating_add(1);
        self.children_by_id.insert(id.clone(), key);
        self.child_order.push(key);

        if let Err(err) = self.spawn_child(key) {
            self.children_by_id.remove(&id);
            self.child_order.retain(|&existing| existing != key);
            self.children.remove(key);
            return Err(map_supervisor_error_to_control(err));
        }

        Ok(())
    }

    pub(crate) fn finish_with_exit(&mut self, exit: SupervisorExit) {
        self.state = SupervisorState::Stopped;
        self.last_exit = Some(exit);
        self.send_event(SupervisorEvent::SupervisorStopped);
    }

    fn handle_nested_snapshot(&mut self, notification: NestedSnapshotNotification) {
        let Some(entry) = self.children.get_mut(notification.parent_key) else {
            return;
        };
        if entry.instance != notification.parent_instance
            || entry.runtime.generation != notification.generation
        {
            return;
        }

        let Some(state) = entry.nested_snapshot_state.clone() else {
            return;
        };
        state.mark_dequeued();
        entry.nested_snapshot = state.latest();
        self.publish_snapshot();
    }

    async fn remove_child(&mut self, id: String) -> Result<(), ControlError> {
        if self.state == SupervisorState::Stopping {
            return Err(ControlError::SupervisorStopping);
        }

        let Some(&key) = self.children_by_id.get(&id) else {
            return Err(ControlError::UnknownChildId(id));
        };

        if self.children[key].membership == MembershipState::Removing {
            return Err(ControlError::ChildRemovalInProgress(id));
        }

        if self.active_child_count() <= 1 {
            return Err(ControlError::LastChildRemovalUnsupported);
        }

        let (mode, grace, active, was_running) = {
            let entry = &mut self.children[key];
            let was_running = counts_as_running(entry.membership, entry.runtime.state);
            entry.membership = MembershipState::Removing;
            let active = entry.runtime.state.is_active();
            if active {
                entry.runtime.state = RuntimeChildState::Stopping;
            }
            (
                entry.runtime.spec.shutdown_policy.mode,
                entry.runtime.spec.shutdown_policy.grace,
                active,
                was_running,
            )
        };
        if was_running {
            self.running_children = self.running_children.saturating_sub(1);
        }

        self.send_event(SupervisorEvent::ChildRemoveRequested { id: id.clone() });

        if !active {
            self.finalize_removed_child(key);
            return Ok(());
        }

        match mode {
            crate::shutdown::ShutdownMode::Abort => {
                self.abort_and_detach_child(key, DrainReason::Shutdown)
                    .await
                    .map_err(map_supervisor_error_to_control)?;
                Ok(())
            }
            crate::shutdown::ShutdownMode::Cooperative => {
                self.cancel_child(key);
                self.await_child_removal(key, Instant::now() + grace, true)
                    .await
            }
            crate::shutdown::ShutdownMode::CooperativeThenAbort => {
                self.cancel_child(key);
                self.await_child_removal(key, Instant::now() + grace, false)
                    .await
            }
        }
    }

    async fn await_child_removal(
        &mut self,
        key: ChildKey,
        deadline: Instant,
        timeout_is_error: bool,
    ) -> Result<(), ControlError> {
        let child_id = self.child_id(key).ok_or_else(|| {
            ControlError::Internal("missing child id while removing child".to_owned())
        })?;
        let started_at = StdInstant::now();
        let mut removal_error = None;

        loop {
            if !self.children.contains(key)
                || self.children[key].membership == MembershipState::Removed
            {
                self.meta.observability.record_shutdown_duration(
                    "remove_child",
                    started_at.elapsed(),
                    Some(&child_id),
                );
                return removal_error.map_or(Ok(()), Err);
            }

            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    self.handle_shutdown_during_control(changed).await?;
                }
                maybe = self.join_set.join_next_with_id() => {
                    self.handle_join_during_control(maybe).await?;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    self.meta
                        .observability
                        .record_shutdown_timeout("remove_child", Some(&child_id));
                    if timeout_is_error {
                        removal_error = Some(ControlError::ShutdownTimedOut(child_id.clone()));
                    }
                    self.abort_and_detach_child(key, DrainReason::Shutdown)
                        .await
                        .map_err(map_supervisor_error_to_control)?;
                    self.meta.observability.record_shutdown_duration(
                        "remove_child",
                        started_at.elapsed(),
                        Some(&child_id),
                    );
                    return removal_error.map_or(Ok(()), Err);
                }
            }
        }
    }

    async fn handle_shutdown_during_control(
        &mut self,
        changed: Result<(), tokio::sync::watch::error::RecvError>,
    ) -> Result<(), ControlError> {
        if self.shutdown_requested(changed) {
            self.queue_shutdown_exit().await;
            Err(ControlError::SupervisorStopping)
        } else {
            Ok(())
        }
    }

    async fn handle_join_during_control(
        &mut self,
        maybe: Option<Result<(Id, ChildEnvelope), JoinError>>,
    ) -> Result<(), ControlError> {
        let Some(joined) = maybe else {
            return Err(ControlError::Internal(
                "supervisor join set drained before child removal completed".to_owned(),
            ));
        };

        self.handle_joined_child(joined)
            .await
            .map_err(map_supervisor_error_to_control)?;

        if self.pending_exit.is_some() {
            return Err(ControlError::SupervisorStopping);
        }

        Ok(())
    }
    fn cancel_child(&mut self, key: ChildKey) {
        if let Some(token) = self.children[key].runtime.active_token.as_ref() {
            token.cancel();
        }
    }

    fn abort_child(&mut self, key: ChildKey) {
        if let Some(abort_handle) = self.children[key].runtime.abort_handle.as_ref() {
            abort_handle.abort();
        }
    }

    fn active_child_count(&self) -> usize {
        self.children_by_id.len()
    }

    fn shutdown_requested(
        &self,
        changed: Result<(), tokio::sync::watch::error::RecvError>,
    ) -> bool {
        match changed {
            Ok(()) => *self.shutdown_rx.borrow(),
            Err(_) => true,
        }
    }

    async fn queue_shutdown_exit(&mut self) {
        self.pending_exit = Some(self.shutdown_all().await);
    }

    fn child_id(&self, key: ChildKey) -> Option<String> {
        self.children.get(key).map(|entry| entry.id.clone())
    }

    pub(crate) fn child_path(&self, key: ChildKey) -> Vec<String> {
        let mut path = self.meta.path_prefix.clone();
        path.push(self.children[key].id.clone());
        path
    }

    fn finalize_removed_child(&mut self, key: ChildKey) {
        if !self.children.contains(key) {
            return;
        }

        let had_live_task = self.children[key].runtime.abort_handle.is_some();
        let mut entry = self.children.remove(key);
        entry.membership = MembershipState::Removed;
        entry.terminal_status = None;
        entry.last_exit = None;
        entry.nested_snapshot = None;
        if let Some(state) = entry.nested_snapshot_state.as_ref() {
            state.clear();
        }
        if had_live_task {
            self.live_tasks = self.live_tasks.saturating_sub(1);
        }
        self.children_by_id.remove(&entry.id);
        self.child_order.retain(|&existing| existing != key);
        self.send_event(SupervisorEvent::ChildRemoved { id: entry.id });
    }

    async fn handle_joined_child(
        &mut self,
        joined: Result<(Id, ChildEnvelope), JoinError>,
    ) -> Result<(), SupervisorError> {
        let Some(classified) = self.consume_joined_child(joined)? else {
            return Ok(());
        };
        self.record_exit(classified.key, classified.generation, &classified.status);

        if self.state == SupervisorState::Stopping {
            return Ok(());
        }

        if self.children[classified.key].membership == MembershipState::Removing {
            self.finalize_removed_child(classified.key);
            return Ok(());
        }

        let restart_policy = self.children[classified.key].runtime.spec.restart;

        if restart_policy.should_restart(classified.status.is_failure()) {
            match self.meta.strategy {
                Strategy::OneForOne => {
                    self.handle_one_for_one_restart(classified.key, classified.generation)
                        .await?
                }
                Strategy::OneForAll => self.handle_one_for_all_restart(classified.key).await?,
            }
        } else {
            self.record_terminal_status(classified.key, classified.generation, &classified.status);
        }

        Ok(())
    }

    pub(crate) fn handle_drained_join(
        &mut self,
        joined: Result<(Id, ChildEnvelope), JoinError>,
        reason: DrainReason,
    ) -> Result<(), SupervisorError> {
        let Some(classified) = self.consume_joined_child(joined)? else {
            return Ok(());
        };
        self.record_exit(classified.key, classified.generation, &classified.status);
        if matches!(reason, DrainReason::GroupRestart)
            && matches!(
                self.children[classified.key].runtime.spec.restart,
                Restart::Temporary
            )
        {
            self.record_terminal_status(classified.key, classified.generation, &classified.status);
        }
        Ok(())
    }

    fn classify_join(
        &mut self,
        joined: Result<(Id, ChildEnvelope), JoinError>,
    ) -> Result<ClassifiedExit, SupervisorError> {
        match joined {
            Ok((task_id, envelope)) => {
                self.task_map.remove(&task_id);
                Ok(ClassifiedExit {
                    key: envelope.key,
                    instance: envelope.instance,
                    generation: envelope.generation,
                    status: ExitStatus::from_child_result(envelope.result),
                })
            }
            Err(err) => {
                let task_id = err.id();
                let Some(meta) = self.task_map.remove(&task_id) else {
                    return Err(SupervisorError::Internal(format!(
                        "missing task metadata for failed join: {err}"
                    )));
                };
                let status = classify_join_error(err);
                Ok(ClassifiedExit {
                    key: meta.key,
                    instance: meta.instance,
                    generation: meta.generation,
                    status,
                })
            }
        }
    }

    fn record_exit(&mut self, key: ChildKey, generation: u64, status: &ExitStatus) {
        if counts_as_running(
            self.children[key].membership,
            self.children[key].runtime.state,
        ) {
            self.running_children = self.running_children.saturating_sub(1);
        }

        let id = {
            let entry = &mut self.children[key];
            entry.runtime.state = RuntimeChildState::Stopped;
            entry.runtime.active_token = None;
            entry.runtime.abort_handle = None;
            entry.runtime.next_restart_deadline = None;
            entry.last_exit = Some(status.view());
            entry.nested_snapshot = None;
            entry.nested_snapshot_state = None;
            entry.id.clone()
        };
        self.send_event(SupervisorEvent::ChildExited {
            id,
            generation,
            status: status.view(),
        });
    }

    fn record_terminal_status(&mut self, key: ChildKey, generation: u64, status: &ExitStatus) {
        let terminal_status = TerminalStatus::new(generation, status.is_failure());
        match &mut self.children[key].terminal_status {
            Some(current) if current.generation > generation => {}
            slot => *slot = Some(terminal_status),
        }
    }

    pub(crate) fn clear_terminal_status(&mut self, key: ChildKey) {
        self.children[key].terminal_status = None;
    }

    async fn handle_one_for_one_restart(
        &mut self,
        key: ChildKey,
        previous_generation: u64,
    ) -> Result<(), SupervisorError> {
        let delay = self.schedule_restart(key)?;
        self.send_event(SupervisorEvent::ChildRestartScheduled {
            id: self.children[key].id.clone(),
            generation: previous_generation,
            delay,
        });
        if !self.wait_for_restart_delay(delay).await? {
            return Ok(());
        }
        if self.children[key].membership != MembershipState::Active {
            return Ok(());
        }
        let (old_generation, new_generation) = self.spawn_child(key)?;
        self.send_restart_event(
            key,
            old_generation.unwrap_or(previous_generation),
            new_generation,
        );
        Ok(())
    }

    async fn handle_one_for_all_restart(
        &mut self,
        failing_key: ChildKey,
    ) -> Result<(), SupervisorError> {
        let delay = self.schedule_restart(failing_key)?;
        self.send_event(SupervisorEvent::GroupRestartScheduled { delay });
        if !self.wait_for_restart_delay(delay).await? {
            return Ok(());
        }

        debug!(
            "restarting child group after exit from {}",
            self.children[failing_key].id
        );
        self.drain_for_group_restart().await?;
        self.group_token = CancellationToken::new();
        let keys = self.child_order.clone();
        for key in keys {
            let entry = &self.children[key];
            if entry.membership != MembershipState::Active
                || matches!(entry.runtime.spec.restart, Restart::Temporary)
            {
                continue;
            }
            let (old_generation, new_generation) = self.spawn_child(key)?;
            if let Some(old_generation) = old_generation {
                self.send_restart_event(key, old_generation, new_generation);
            }
        }
        Ok(())
    }

    fn schedule_restart(&mut self, key: ChildKey) -> Result<Duration, SupervisorError> {
        let delay = {
            let now = Instant::now();
            let child = &mut self.children[key].runtime;
            child.restart_tracker.record(now);
            if child.restart_tracker.exceeded() {
                None
            } else {
                let delay = child.restart_tracker.backoff();
                child.next_restart_deadline = Some(now + delay);
                Some(delay)
            }
        };

        let Some(delay) = delay else {
            self.send_event(SupervisorEvent::RestartIntensityExceeded);
            return Err(SupervisorError::RestartIntensityExceeded);
        };

        let child_id = &*self.children[key].id;
        trace!(?child_id, ?delay, "scheduled child restart");
        Ok(delay)
    }

    async fn wait_for_restart_delay(&mut self, delay: Duration) -> Result<bool, SupervisorError> {
        if delay.is_zero() {
            if *self.shutdown_rx.borrow() {
                self.queue_shutdown_exit().await;
                return Ok(false);
            }

            tokio::task::yield_now().await;

            if *self.shutdown_rx.borrow() {
                self.queue_shutdown_exit().await;
                return Ok(false);
            }

            while self.commands_open {
                match self.command_rx.try_recv() {
                    Ok(command) => Box::pin(self.handle_command(command)).await,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        self.commands_open = false;
                        break;
                    }
                }

                if self.pending_exit.is_some() {
                    return Ok(false);
                }
            }

            return Ok(true);
        }

        let deadline = Instant::now() + delay;
        loop {
            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    if self.shutdown_requested(changed) {
                        self.queue_shutdown_exit().await;
                        return Ok(false);
                    }
                }
                command = self.command_rx.recv(), if self.commands_open => {
                    match command {
                        Some(command) => Box::pin(self.handle_command(command)).await,
                        None => self.commands_open = false,
                    }
                    if self.pending_exit.is_some() {
                        return Ok(false);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => return Ok(true),
            }
        }
    }

    fn no_running_children(&self) -> bool {
        self.running_children == 0
    }

    fn running_child_count(&self) -> usize {
        self.running_children
    }

    fn send_restart_event(&self, key: ChildKey, old_generation: u64, new_generation: u64) {
        self.send_event(SupervisorEvent::ChildRestarted {
            id: self.children[key].id.clone(),
            old_generation,
            new_generation,
        });
    }

    pub(crate) fn send_event(&self, event: SupervisorEvent) {
        if event_updates_snapshot(&event) {
            self.publish_snapshot();
        }
        let child_path = event_child_id(&event)
            .and_then(|id| self.children_by_id.get(id))
            .map(|&key| self.children[key].formatted_path.as_str());
        self.meta
            .observability
            .emit_event(&event, self.running_child_count(), child_path);
        let _ = self.events.send(event);
    }

    pub(crate) fn publish_snapshot(&self) {
        let _ = self.snapshots.send_replace(self.snapshot_view());
    }

    fn snapshot_view(&self) -> SupervisorSnapshot {
        let now = Instant::now();
        let mut children = Vec::with_capacity(self.children_by_id.len());
        for &key in &self.child_order {
            let Some(entry) = self.children.get(key) else {
                continue;
            };

            children.push(ChildSnapshot {
                id: entry.id.clone(),
                generation: entry.runtime.generation,
                state: match entry.runtime.state {
                    RuntimeChildState::Starting => ChildStateView::Starting,
                    RuntimeChildState::Running => ChildStateView::Running,
                    RuntimeChildState::Stopping => ChildStateView::Stopping,
                    RuntimeChildState::Stopped => ChildStateView::Stopped,
                },
                membership: match entry.membership {
                    MembershipState::Active => ChildMembershipView::Active,
                    MembershipState::Removing => ChildMembershipView::Removing,
                    MembershipState::Removed => unreachable!("removed children filtered"),
                },
                last_exit: entry.last_exit.clone(),
                restart_count: entry.runtime.restart_tracker.total_restarts(),
                next_restart_in: entry
                    .runtime
                    .next_restart_deadline
                    .map(|deadline| deadline.saturating_duration_since(now)),
                supervisor: entry.nested_snapshot.as_ref().cloned().map(Box::new),
            });
        }

        SupervisorSnapshot {
            state: match self.state {
                SupervisorState::Running => SupervisorStateView::Running,
                SupervisorState::Stopping => SupervisorStateView::Stopping,
                SupervisorState::Stopped => SupervisorStateView::Stopped,
            },
            last_exit: self.last_exit,
            strategy: self.meta.strategy,
            children,
        }
    }

    pub(crate) async fn drain_ready_joins(
        &mut self,
        reason: DrainReason,
    ) -> Result<(), SupervisorError> {
        loop {
            match tokio::time::timeout(Duration::ZERO, self.join_set.join_next_with_id()).await {
                Ok(Some(joined)) => self.handle_drained_join(joined, reason)?,
                Ok(None) | Err(_) => return Ok(()),
            }
        }
    }

    async fn abort_and_detach_child(
        &mut self,
        key: ChildKey,
        reason: DrainReason,
    ) -> Result<(), SupervisorError> {
        self.abort_child(key);
        tokio::task::yield_now().await;
        self.drain_ready_joins(reason).await?;
        self.finalize_removed_child_if_present(key);
        Ok(())
    }

    fn consume_joined_child(
        &mut self,
        joined: Result<(Id, ChildEnvelope), JoinError>,
    ) -> Result<Option<ClassifiedExit>, SupervisorError> {
        let classified = self.classify_join(joined)?;
        if !self.current_child_matches(classified.key, classified.instance, classified.generation) {
            return Ok(None);
        }

        self.live_tasks = self.live_tasks.saturating_sub(1);
        Ok(Some(classified))
    }

    fn current_child_matches(&self, key: ChildKey, instance: u64, generation: u64) -> bool {
        self.children.get(key).is_some_and(|entry| {
            entry.instance == instance && entry.runtime.generation == generation
        })
    }

    fn finalize_removed_child_if_present(&mut self, key: ChildKey) {
        if self.children.contains(key) {
            self.finalize_removed_child(key);
        }
    }
}

fn classify_join_error(err: JoinError) -> ExitStatus {
    if err.is_cancelled() {
        ExitStatus::Aborted
    } else {
        ExitStatus::Panicked
    }
}

fn map_build_error_to_control(id: &str, err: crate::error::BuildError) -> ControlError {
    match err {
        crate::error::BuildError::DuplicateChildId(_) => {
            ControlError::DuplicateChildId(id.to_owned())
        }
        crate::error::BuildError::EmptyChildren => {
            ControlError::InvalidConfig("supervisor requires at least one child")
        }
        crate::error::BuildError::InvalidConfig(message) => ControlError::InvalidConfig(message),
    }
}

fn map_supervisor_error_to_control(err: SupervisorError) -> ControlError {
    match err {
        SupervisorError::ShutdownTimedOut(ids) => ControlError::ShutdownTimedOut(ids),
        SupervisorError::Internal(message) => ControlError::Internal(message),
        SupervisorError::RestartIntensityExceeded => ControlError::SupervisorStopping,
    }
}

struct ClassifiedExit {
    key: ChildKey,
    instance: u64,
    generation: u64,
    status: ExitStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalStatus {
    generation: u64,
    is_failure: bool,
}

impl TerminalStatus {
    fn new(generation: u64, is_failure: bool) -> Self {
        Self {
            generation,
            is_failure,
        }
    }

    fn is_failure(&self) -> bool {
        self.is_failure
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DrainReason {
    Shutdown,
    GroupRestart,
}

fn counts_as_running(membership: MembershipState, state: RuntimeChildState) -> bool {
    membership != MembershipState::Removed
        && matches!(
            state,
            RuntimeChildState::Starting | RuntimeChildState::Running
        )
}

fn event_updates_snapshot(event: &SupervisorEvent) -> bool {
    !matches!(
        event,
        SupervisorEvent::SupervisorStarted | SupervisorEvent::ChildRestarted { .. }
    )
}

fn event_child_id(event: &SupervisorEvent) -> Option<&str> {
    match event {
        SupervisorEvent::ChildStarted { id, .. }
        | SupervisorEvent::ChildExited { id, .. }
        | SupervisorEvent::ChildRestartScheduled { id, .. }
        | SupervisorEvent::ChildRestarted { id, .. }
        | SupervisorEvent::ChildRemoveRequested { id }
        | SupervisorEvent::ChildRemoved { id }
        | SupervisorEvent::Nested { id, .. } => Some(id),
        SupervisorEvent::SupervisorStarted
        | SupervisorEvent::SupervisorStopping
        | SupervisorEvent::SupervisorStopped
        | SupervisorEvent::GroupRestartScheduled { .. }
        | SupervisorEvent::RestartIntensityExceeded => None,
    }
}
