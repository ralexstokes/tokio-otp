use std::{collections::HashMap, sync::Arc, time::Duration};

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
    restart::{Restart, RestartIntensity},
    snapshot::{
        ChildMembershipView, ChildSnapshot, ChildStateView, NestedSnapshotUpdate,
        SupervisorSnapshot, SupervisorStateView,
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
    pub(crate) generation: u64,
    pub(crate) result: crate::child::ChildResult,
}

#[derive(Clone, Copy)]
pub(crate) struct TaskMeta {
    pub(crate) key: ChildKey,
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
    pub(crate) runtime: ChildRuntime,
    terminal_status: Option<TerminalStatus>,
    last_exit: Option<ExitStatusView>,
    pub(crate) nested_snapshot: Option<SupervisorSnapshot>,
    pub(crate) membership: MembershipState,
}

impl ChildEntry {
    fn new(
        id: String,
        spec: Arc<crate::child::ChildSpecInner>,
        default_restart_intensity: RestartIntensity,
    ) -> Self {
        Self {
            id,
            runtime: ChildRuntime::new(spec, default_restart_intensity),
            terminal_status: None,
            last_exit: None,
            nested_snapshot: None,
            membership: MembershipState::Active,
        }
    }
}

pub(crate) struct SupervisorRuntime {
    pub(crate) strategy: Strategy,
    pub(crate) default_restart_intensity: RestartIntensity,
    pub(crate) state: SupervisorState,
    pub(crate) group_token: CancellationToken,
    pub(crate) join_set: JoinSet<ChildEnvelope>,
    pub(crate) children: Vec<ChildEntry>,
    pub(crate) children_by_id: HashMap<String, ChildKey>,
    pub(crate) registry: Arc<NestedControlRegistry>,
    pub(crate) path_prefix: Vec<String>,
    pub(crate) events: broadcast::Sender<SupervisorEvent>,
    pub(crate) snapshots: watch::Sender<SupervisorSnapshot>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    pub(crate) command_rx: mpsc::UnboundedReceiver<SupervisorCommand>,
    pub(crate) nested_snapshot_tx: mpsc::UnboundedSender<NestedSnapshotUpdate>,
    pub(crate) nested_snapshot_rx: mpsc::UnboundedReceiver<NestedSnapshotUpdate>,
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
        command_rx: mpsc::UnboundedReceiver<SupervisorCommand>,
        registry: Arc<NestedControlRegistry>,
        path_prefix: Vec<String>,
    ) -> Self {
        let default_restart_intensity = config.restart_intensity;
        let mut children = Vec::with_capacity(config.children.len());
        let mut children_by_id = HashMap::with_capacity(config.children.len());

        for (key, spec) in config.children.into_iter().enumerate() {
            let id = spec.id.clone();
            children_by_id.insert(id.clone(), key);
            children.push(ChildEntry::new(id, spec, default_restart_intensity));
        }
        let (nested_snapshot_tx, nested_snapshot_rx) = mpsc::unbounded_channel();

        Self {
            strategy: config.strategy,
            default_restart_intensity,
            state: SupervisorState::Running,
            group_token: CancellationToken::new(),
            join_set: JoinSet::new(),
            children,
            children_by_id,
            registry,
            path_prefix,
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
        for key in 0..self.children.len() {
            self.spawn_child(key)?;
        }

        loop {
            if self.join_set.is_empty() && self.no_running_children() {
                return self.finish_natural_exit();
            }

            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    match changed {
                        Ok(()) if *self.shutdown_rx.borrow() => return self.shutdown_all().await,
                        Ok(()) => {}
                        Err(_) => return self.shutdown_all().await,
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
            .children
            .iter()
            .filter(|entry| entry.membership != MembershipState::Removed)
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

        if let Some(restart_intensity) = child.restart_intensity_override() {
            restart_intensity
                .validate()
                .map_err(|err| map_build_error_to_control(child.id(), err))?;
        }

        let id = child.id().to_owned();
        if self.children_by_id.contains_key(&id) {
            return Err(ControlError::DuplicateChildId(id));
        }

        let key = self.children.len();
        self.children.push(ChildEntry::new(
            id.clone(),
            Arc::clone(&child.inner),
            self.default_restart_intensity,
        ));
        self.children_by_id.insert(id.clone(), key);

        if let Err(err) = self.spawn_child(key) {
            self.children_by_id.remove(&id);
            self.children[key].membership = MembershipState::Removed;
            return Err(map_supervisor_error_to_control(err));
        }

        Ok(())
    }

    pub(crate) fn finish_with_exit(&mut self, exit: SupervisorExit) {
        self.state = SupervisorState::Stopped;
        self.last_exit = Some(exit);
        self.send_event(SupervisorEvent::SupervisorStopped);
    }

    fn handle_nested_snapshot(&mut self, update: NestedSnapshotUpdate) {
        let Some(&key) = self.children_by_id.get(&update.parent_id) else {
            return;
        };

        let entry = &mut self.children[key];
        if entry.membership == MembershipState::Removed
            || entry.runtime.generation != update.generation
        {
            return;
        }

        entry.nested_snapshot = Some(update.snapshot);
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

        let (mode, grace, active) = {
            let entry = &mut self.children[key];
            entry.membership = MembershipState::Removing;
            let active = entry.runtime.state.is_active();
            if active {
                entry.runtime.state = RuntimeChildState::Stopping;
            }
            (
                entry.runtime.spec.shutdown_policy.mode,
                entry.runtime.spec.shutdown_policy.grace,
                active,
            )
        };

        self.send_event(SupervisorEvent::ChildRemoveRequested { id: id.clone() });

        if !active {
            self.finalize_removed_child(key);
            return Ok(());
        }

        let (deadline, timeout_is_error) = match mode {
            crate::shutdown::ShutdownMode::Abort => {
                self.abort_child(key);
                (None, false)
            }
            crate::shutdown::ShutdownMode::Cooperative => {
                self.cancel_child(key);
                (Some(Instant::now() + grace), true)
            }
            crate::shutdown::ShutdownMode::CooperativeThenAbort => {
                self.cancel_child(key);
                (Some(Instant::now() + grace), false)
            }
        };

        self.await_child_removal(key, deadline, timeout_is_error)
            .await
    }

    async fn await_child_removal(
        &mut self,
        key: ChildKey,
        deadline: Option<Instant>,
        timeout_is_error: bool,
    ) -> Result<(), ControlError> {
        let child_id = self.child_id(key).ok_or_else(|| {
            ControlError::Internal("missing child id while removing child".to_owned())
        })?;
        let mut deadline = deadline;
        let mut removal_error = None;

        loop {
            if self.children[key].membership == MembershipState::Removed {
                return removal_error.map_or(Ok(()), Err);
            }

            if let Some(deadline_at) = deadline {
                tokio::select! {
                    biased;
                    changed = self.shutdown_rx.changed() => {
                        self.handle_shutdown_during_control(changed).await?;
                    }
                    maybe = self.join_set.join_next_with_id() => {
                        self.handle_join_during_control(maybe).await?;
                    }
                    _ = tokio::time::sleep_until(deadline_at) => {
                        self.abort_child(key);
                        if timeout_is_error {
                            removal_error = Some(ControlError::ShutdownTimedOut(child_id.clone()));
                        }
                        deadline = None;
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    changed = self.shutdown_rx.changed() => {
                        self.handle_shutdown_during_control(changed).await?;
                    }
                    maybe = self.join_set.join_next_with_id() => {
                        self.handle_join_during_control(maybe).await?;
                    }
                }
            }
        }
    }

    async fn handle_shutdown_during_control(
        &mut self,
        changed: Result<(), tokio::sync::watch::error::RecvError>,
    ) -> Result<(), ControlError> {
        match changed {
            Ok(()) if *self.shutdown_rx.borrow() => {
                let result = self.shutdown_all().await;
                self.pending_exit = Some(result);
                Err(ControlError::SupervisorStopping)
            }
            Ok(()) => Ok(()),
            Err(_) => {
                let result = self.shutdown_all().await;
                self.pending_exit = Some(result);
                Err(ControlError::SupervisorStopping)
            }
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

        if let Some(result) = self.pending_exit.take() {
            self.pending_exit = Some(result);
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

    fn child_id(&self, key: ChildKey) -> Option<String> {
        self.children.get(key).map(|entry| entry.id.clone())
    }

    pub(crate) fn child_path(&self, key: ChildKey) -> Vec<String> {
        let mut path = self.path_prefix.clone();
        path.push(self.children[key].id.clone());
        path
    }

    fn finalize_removed_child(&mut self, key: ChildKey) {
        if self.children[key].membership == MembershipState::Removed {
            return;
        }

        let id = {
            let entry = &mut self.children[key];
            entry.membership = MembershipState::Removed;
            entry.terminal_status = None;
            entry.last_exit = None;
            entry.nested_snapshot = None;
            entry.id.clone()
        };
        self.children_by_id.remove(&id);
        self.send_event(SupervisorEvent::ChildRemoved { id });
    }

    async fn handle_joined_child(
        &mut self,
        joined: Result<(Id, ChildEnvelope), JoinError>,
    ) -> Result<(), SupervisorError> {
        let classified = self.classify_join(joined)?;
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
            match self.strategy {
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
        let classified = self.classify_join(joined)?;
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
                    generation: meta.generation,
                    status,
                })
            }
        }
    }

    fn record_exit(&mut self, key: ChildKey, generation: u64, status: &ExitStatus) {
        let id = {
            let entry = &mut self.children[key];
            entry.runtime.state = RuntimeChildState::Stopped;
            entry.runtime.active_token = None;
            entry.runtime.abort_handle = None;
            entry.runtime.next_restart_deadline = None;
            entry.last_exit = Some(status.view());
            entry.nested_snapshot = None;
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
        for key in 0..self.children.len() {
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
            let child = &mut self.children[key].runtime;
            child.restart_tracker.record(Instant::now());
            if child.restart_tracker.exceeded() {
                None
            } else {
                let delay = child.restart_tracker.backoff();
                child.next_restart_deadline = Some(Instant::now() + delay);
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
        tokio::select! {
            biased;
            changed = self.shutdown_rx.changed() => {
                match changed {
                    Ok(()) if *self.shutdown_rx.borrow() => {
                        let result = self.shutdown_all().await;
                        self.pending_exit = Some(result);
                        Ok(false)
                    }
                    Ok(()) => Ok(true),
                    Err(_) => {
                        let result = self.shutdown_all().await;
                        self.pending_exit = Some(result);
                        Ok(false)
                    }
                }
            }
            _ = tokio::time::sleep(delay) => Ok(true),
        }
    }

    fn no_running_children(&self) -> bool {
        self.children.iter().all(|entry| {
            entry.membership == MembershipState::Removed
                || entry.runtime.state == RuntimeChildState::Stopped
        })
    }

    fn send_restart_event(&self, key: ChildKey, old_generation: u64, new_generation: u64) {
        self.send_event(SupervisorEvent::ChildRestarted {
            id: self.children[key].id.clone(),
            old_generation,
            new_generation,
        });
    }

    pub(crate) fn send_event(&self, event: SupervisorEvent) {
        self.publish_snapshot();
        let _ = self.events.send(event);
    }

    pub(crate) fn publish_snapshot(&self) {
        let _ = self.snapshots.send_replace(self.snapshot_view());
    }

    fn snapshot_view(&self) -> SupervisorSnapshot {
        SupervisorSnapshot {
            state: match self.state {
                SupervisorState::Running => SupervisorStateView::Running,
                SupervisorState::Stopping => SupervisorStateView::Stopping,
                SupervisorState::Stopped => SupervisorStateView::Stopped,
            },
            last_exit: self.last_exit,
            strategy: self.strategy,
            children: self
                .children
                .iter()
                .filter(|entry| entry.membership != MembershipState::Removed)
                .map(|entry| ChildSnapshot {
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
                        .map(|deadline| deadline.saturating_duration_since(Instant::now())),
                    supervisor: entry.nested_snapshot.as_ref().cloned().map(Box::new),
                })
                .collect(),
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
