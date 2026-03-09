use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use tokio::{
    sync::{broadcast, watch},
    task::{Id, JoinError, JoinSet},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::{
    child::ChildResult,
    error::{SupervisorError, SupervisorExit},
    event::SupervisorEvent,
    restart::Restart,
    strategy::Strategy,
    supervisor::SupervisorConfig,
};

use super::{
    child_runtime::{ChildRuntime, RuntimeChildState},
    exit::ExitStatus,
    intensity::{compute_backoff, intensity_exceeded, prune_restart_window, record_restart},
};

pub(crate) struct ChildEnvelope {
    pub(crate) id: String,
    pub(crate) generation: u64,
    pub(crate) result: ChildResult,
}

#[derive(Clone)]
pub(crate) struct TaskMeta {
    pub(crate) id: String,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SupervisorState {
    Running,
    Stopping,
}

pub(crate) struct SupervisorRuntime {
    pub(crate) strategy: Strategy,
    pub(crate) intensity: crate::restart::RestartIntensity,
    pub(crate) state: SupervisorState,
    pub(crate) group_token: CancellationToken,
    pub(crate) join_set: JoinSet<ChildEnvelope>,
    pub(crate) children: HashMap<String, ChildRuntime>,
    pub(crate) restart_times: VecDeque<Instant>,
    pub(crate) events: broadcast::Sender<SupervisorEvent>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    pub(crate) task_map: HashMap<Id, TaskMeta>,
    terminal_statuses: HashMap<String, TerminalStatus>,
    pending_exit: Option<Result<SupervisorExit, SupervisorError>>,
}

impl SupervisorRuntime {
    pub(crate) fn new(
        config: SupervisorConfig,
        shutdown_rx: watch::Receiver<bool>,
        events: broadcast::Sender<SupervisorEvent>,
    ) -> Self {
        let children = config
            .children
            .into_iter()
            .map(|spec| {
                let id = spec.id.clone();
                (id, ChildRuntime::new(spec))
            })
            .collect();

        Self {
            strategy: config.strategy,
            intensity: config.restart_intensity,
            state: SupervisorState::Running,
            group_token: CancellationToken::new(),
            join_set: JoinSet::new(),
            children,
            restart_times: VecDeque::new(),
            events,
            shutdown_rx,
            task_map: HashMap::new(),
            terminal_statuses: HashMap::new(),
            pending_exit: None,
        }
    }

    pub(crate) async fn run(&mut self) -> Result<SupervisorExit, SupervisorError> {
        self.send_event(SupervisorEvent::SupervisorStarted);
        let initial_ids: Vec<String> = self.children.keys().cloned().collect();
        for id in initial_ids {
            self.spawn_child(&id)?;
        }

        loop {
            if self.join_set.is_empty() && self.no_running_children() {
                return self.finish_natural_exit();
            }

            tokio::select! {
                changed = self.shutdown_rx.changed() => {
                    match changed {
                        Ok(()) if *self.shutdown_rx.borrow() => return self.shutdown_all().await,
                        Ok(()) => {}
                        Err(_) => return self.shutdown_all().await,
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
        self.send_event(SupervisorEvent::SupervisorStopped);
        if self
            .terminal_statuses
            .values()
            .any(TerminalStatus::is_failure)
        {
            Ok(SupervisorExit::Failed)
        } else {
            Ok(SupervisorExit::Completed)
        }
    }

    async fn handle_joined_child(
        &mut self,
        joined: Result<(Id, ChildEnvelope), JoinError>,
    ) -> Result<(), SupervisorError> {
        let classified = self.classify_join(joined)?;
        self.record_exit(&classified.id, classified.generation, &classified.status);

        if self.state == SupervisorState::Stopping {
            return Ok(());
        }

        let restart_policy = self
            .children
            .get(&classified.id)
            .map(|child| child.spec.restart)
            .ok_or_else(|| {
                SupervisorError::Internal(format!("missing child runtime for {}", classified.id))
            })?;

        if should_restart(restart_policy, &classified.status) {
            match self.strategy {
                Strategy::OneForOne => {
                    self.handle_one_for_one_restart(classified.id, classified.generation)
                        .await?
                }
                Strategy::OneForAll => {
                    self.handle_one_for_all_restart(classified.id, classified.generation)
                        .await?
                }
            }
        } else {
            self.record_terminal_status(&classified.id, classified.generation, &classified.status);
        }

        Ok(())
    }

    pub(crate) fn handle_drained_join(
        &mut self,
        joined: Result<(Id, ChildEnvelope), JoinError>,
        reason: DrainReason,
    ) -> Result<(), SupervisorError> {
        let classified = self.classify_join(joined)?;
        self.record_exit(&classified.id, classified.generation, &classified.status);
        if matches!(reason, DrainReason::GroupRestart)
            && !self.should_respawn_on_group_restart(&classified.id)?
        {
            self.record_terminal_status(&classified.id, classified.generation, &classified.status);
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
                    id: envelope.id,
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
                    id: meta.id,
                    generation: meta.generation,
                    status,
                })
            }
        }
    }

    fn record_exit(&mut self, id: &str, generation: u64, status: &ExitStatus) {
        if let Some(child) = self.children.get_mut(id) {
            child.state = RuntimeChildState::Stopped;
            child.active_token = None;
            child.abort_handle = None;
        }
        self.send_event(SupervisorEvent::ChildExited {
            id: id.to_owned(),
            generation,
            status: status.view(),
        });
    }

    fn record_terminal_status(&mut self, id: &str, generation: u64, status: &ExitStatus) {
        let terminal_status = TerminalStatus::new(generation, status.is_failure());
        match self.terminal_statuses.get_mut(id) {
            Some(current) if current.generation > generation => {}
            Some(current) => *current = terminal_status,
            None => {
                self.terminal_statuses
                    .insert(id.to_owned(), terminal_status);
            }
        }
    }

    pub(crate) fn clear_terminal_status(&mut self, id: &str) {
        self.terminal_statuses.remove(id);
    }

    fn should_respawn_on_group_restart(&self, id: &str) -> Result<bool, SupervisorError> {
        self.children
            .get(id)
            .map(|child| !matches!(child.spec.restart, Restart::Temporary))
            .ok_or_else(|| SupervisorError::Internal(format!("missing child runtime for {id}")))
    }

    async fn handle_one_for_one_restart(
        &mut self,
        id: String,
        previous_generation: u64,
    ) -> Result<(), SupervisorError> {
        let delay = self.schedule_restart(Some(&id))?;
        self.send_event(SupervisorEvent::ChildRestartScheduled {
            id: id.clone(),
            generation: previous_generation,
            delay,
        });
        if !self.wait_for_restart_delay(delay).await? {
            return Ok(());
        }
        let (old_generation, new_generation) = self.spawn_child(&id)?;
        self.send_restart_event(
            id,
            old_generation.unwrap_or(previous_generation),
            new_generation,
        );
        Ok(())
    }

    async fn handle_one_for_all_restart(
        &mut self,
        failing_id: String,
        _previous_generation: u64,
    ) -> Result<(), SupervisorError> {
        let delay = self.schedule_restart(None)?;
        self.send_event(SupervisorEvent::GroupRestartScheduled { delay });
        if !self.wait_for_restart_delay(delay).await? {
            return Ok(());
        }

        debug!("restarting child group after exit from {}", failing_id);
        self.drain_for_group_restart().await?;
        self.group_token = CancellationToken::new();
        let ids = self.respawnable_child_ids();
        for id in ids {
            let (old_generation, new_generation) = self.spawn_child(&id)?;
            if let Some(old_generation) = old_generation {
                self.send_restart_event(id, old_generation, new_generation);
            }
        }
        Ok(())
    }

    fn schedule_restart(&mut self, child_id: Option<&str>) -> Result<Duration, SupervisorError> {
        let now = Instant::now();
        prune_restart_window(&mut self.restart_times, self.intensity.within, now);
        record_restart(&mut self.restart_times, now);
        if intensity_exceeded(&self.restart_times, self.intensity.max_restarts) {
            self.send_event(SupervisorEvent::RestartIntensityExceeded);
            return Err(SupervisorError::RestartIntensityExceeded);
        }

        let delay = compute_backoff(&self.intensity, self.restart_times.len());
        trace!(?child_id, ?delay, "scheduled child restart");
        Ok(delay)
    }

    async fn wait_for_restart_delay(&mut self, delay: Duration) -> Result<bool, SupervisorError> {
        if delay.is_zero() {
            if *self.shutdown_rx.borrow() {
                let _ = self.shutdown_all().await?;
                return Ok(false);
            }
            return Ok(true);
        }

        tokio::select! {
            _ = tokio::time::sleep(delay) => Ok(true),
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
        }
    }

    fn no_running_children(&self) -> bool {
        self.children
            .values()
            .all(|child| child.state == RuntimeChildState::Stopped)
    }

    fn respawnable_child_ids(&self) -> Vec<String> {
        self.children
            .iter()
            .filter(|(_, child)| !matches!(child.spec.restart, Restart::Temporary))
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn send_restart_event(&self, id: String, old_generation: u64, new_generation: u64) {
        self.send_event(SupervisorEvent::ChildRestarted {
            id,
            old_generation,
            new_generation,
        });
    }

    pub(crate) fn send_event(&self, event: SupervisorEvent) {
        let _ = self.events.send(event);
    }
}

fn classify_join_error(err: JoinError) -> ExitStatus {
    if err.is_cancelled() {
        ExitStatus::Aborted
    } else {
        ExitStatus::Panicked
    }
}

pub(crate) fn should_restart(restart: Restart, status: &ExitStatus) -> bool {
    match restart {
        Restart::Permanent => true,
        Restart::Transient => matches!(
            status,
            ExitStatus::Failed(_) | ExitStatus::Panicked | ExitStatus::Aborted
        ),
        Restart::Temporary => false,
    }
}

struct ClassifiedExit {
    id: String,
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
