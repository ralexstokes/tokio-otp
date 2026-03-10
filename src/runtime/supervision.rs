use std::{collections::HashMap, time::Duration};

use tokio::{
    sync::{broadcast, watch},
    task::{Id, JoinError, JoinSet},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::{
    error::{SupervisorError, SupervisorExit},
    event::SupervisorEvent,
    restart::Restart,
    strategy::Strategy,
    supervisor::SupervisorConfig,
};

use super::{
    child_runtime::{ChildRuntime, RuntimeChildState},
    exit::ExitStatus,
};

pub(crate) struct ChildEnvelope {
    pub(crate) idx: usize,
    pub(crate) generation: u64,
    pub(crate) result: crate::child::ChildResult,
}

#[derive(Clone, Copy)]
pub(crate) struct TaskMeta {
    pub(crate) idx: usize,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SupervisorState {
    Running,
    Stopping,
}

pub(crate) struct SupervisorRuntime {
    pub(crate) strategy: Strategy,
    pub(crate) state: SupervisorState,
    pub(crate) group_token: CancellationToken,
    pub(crate) join_set: JoinSet<ChildEnvelope>,
    pub(crate) child_names: Vec<String>,
    pub(crate) children: Vec<ChildRuntime>,
    pub(crate) events: broadcast::Sender<SupervisorEvent>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    pub(crate) task_map: HashMap<Id, TaskMeta>,
    terminal_statuses: Vec<Option<TerminalStatus>>,
    pending_exit: Option<Result<SupervisorExit, SupervisorError>>,
}

impl SupervisorRuntime {
    pub(crate) fn new(
        config: SupervisorConfig,
        shutdown_rx: watch::Receiver<bool>,
        events: broadcast::Sender<SupervisorEvent>,
    ) -> Self {
        let child_names: Vec<String> = config.children.iter().map(|s| s.id.clone()).collect();
        let child_count = config.children.len();
        let default_restart_intensity = config.restart_intensity;
        let children = config
            .children
            .into_iter()
            .map(|spec| ChildRuntime::new(spec, default_restart_intensity))
            .collect();

        Self {
            strategy: config.strategy,
            state: SupervisorState::Running,
            group_token: CancellationToken::new(),
            join_set: JoinSet::new(),
            child_names,
            children,
            events,
            shutdown_rx,
            task_map: HashMap::new(),
            terminal_statuses: vec![None; child_count],
            pending_exit: None,
        }
    }

    pub(crate) async fn run(&mut self) -> Result<SupervisorExit, SupervisorError> {
        self.send_event(SupervisorEvent::SupervisorStarted);
        for idx in 0..self.children.len() {
            self.spawn_child(idx)?;
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
            .iter()
            .any(|s| s.as_ref().is_some_and(TerminalStatus::is_failure))
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
        self.record_exit(classified.idx, classified.generation, &classified.status);

        if self.state == SupervisorState::Stopping {
            return Ok(());
        }

        let restart_policy = self.children[classified.idx].spec.restart;

        if restart_policy.should_restart(classified.status.is_failure()) {
            match self.strategy {
                Strategy::OneForOne => {
                    self.handle_one_for_one_restart(classified.idx, classified.generation)
                        .await?
                }
                Strategy::OneForAll => self.handle_one_for_all_restart(classified.idx).await?,
            }
        } else {
            self.record_terminal_status(classified.idx, classified.generation, &classified.status);
        }

        Ok(())
    }

    pub(crate) fn handle_drained_join(
        &mut self,
        joined: Result<(Id, ChildEnvelope), JoinError>,
        reason: DrainReason,
    ) -> Result<(), SupervisorError> {
        let classified = self.classify_join(joined)?;
        self.record_exit(classified.idx, classified.generation, &classified.status);
        if matches!(reason, DrainReason::GroupRestart)
            && matches!(
                self.children[classified.idx].spec.restart,
                Restart::Temporary
            )
        {
            self.record_terminal_status(classified.idx, classified.generation, &classified.status);
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
                    idx: envelope.idx,
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
                    idx: meta.idx,
                    generation: meta.generation,
                    status,
                })
            }
        }
    }

    fn record_exit(&mut self, idx: usize, generation: u64, status: &ExitStatus) {
        let child = &mut self.children[idx];
        child.state = RuntimeChildState::Stopped;
        child.active_token = None;
        child.abort_handle = None;
        self.send_event(SupervisorEvent::ChildExited {
            id: self.child_names[idx].clone(),
            generation,
            status: status.view(),
        });
    }

    fn record_terminal_status(&mut self, idx: usize, generation: u64, status: &ExitStatus) {
        let terminal_status = TerminalStatus::new(generation, status.is_failure());
        match &mut self.terminal_statuses[idx] {
            Some(current) if current.generation > generation => {}
            slot => *slot = Some(terminal_status),
        }
    }

    pub(crate) fn clear_terminal_status(&mut self, idx: usize) {
        self.terminal_statuses[idx] = None;
    }

    async fn handle_one_for_one_restart(
        &mut self,
        idx: usize,
        previous_generation: u64,
    ) -> Result<(), SupervisorError> {
        let delay = self.schedule_restart(idx)?;
        self.send_event(SupervisorEvent::ChildRestartScheduled {
            id: self.child_names[idx].clone(),
            generation: previous_generation,
            delay,
        });
        if !self.wait_for_restart_delay(delay).await? {
            return Ok(());
        }
        let (old_generation, new_generation) = self.spawn_child(idx)?;
        self.send_restart_event(
            idx,
            old_generation.unwrap_or(previous_generation),
            new_generation,
        );
        Ok(())
    }

    async fn handle_one_for_all_restart(
        &mut self,
        failing_idx: usize,
    ) -> Result<(), SupervisorError> {
        let delay = self.schedule_restart(failing_idx)?;
        self.send_event(SupervisorEvent::GroupRestartScheduled { delay });
        if !self.wait_for_restart_delay(delay).await? {
            return Ok(());
        }

        debug!(
            "restarting child group after exit from {}",
            self.child_names[failing_idx]
        );
        self.drain_for_group_restart().await?;
        self.group_token = CancellationToken::new();
        for idx in 0..self.children.len() {
            if matches!(self.children[idx].spec.restart, Restart::Temporary) {
                continue;
            }
            let (old_generation, new_generation) = self.spawn_child(idx)?;
            if let Some(old_generation) = old_generation {
                self.send_restart_event(idx, old_generation, new_generation);
            }
        }
        Ok(())
    }

    fn schedule_restart(&mut self, idx: usize) -> Result<Duration, SupervisorError> {
        let delay = {
            let child = &mut self.children[idx];
            child.restart_tracker.record(Instant::now());
            if child.restart_tracker.exceeded() {
                None
            } else {
                Some(child.restart_tracker.backoff())
            }
        };

        let Some(delay) = delay else {
            self.send_event(SupervisorEvent::RestartIntensityExceeded);
            return Err(SupervisorError::RestartIntensityExceeded);
        };

        let child_id = &*self.child_names[idx];
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
        self.children
            .iter()
            .all(|child| child.state == RuntimeChildState::Stopped)
    }

    fn send_restart_event(&self, idx: usize, old_generation: u64, new_generation: u64) {
        self.send_event(SupervisorEvent::ChildRestarted {
            id: self.child_names[idx].clone(),
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

struct ClassifiedExit {
    idx: usize,
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
