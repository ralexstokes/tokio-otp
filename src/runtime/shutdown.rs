use tokio::time::{Instant, sleep_until};

use crate::{
    error::{SupervisorError, SupervisorExit},
    event::SupervisorEvent,
    runtime::{
        child_runtime::RuntimeChildState,
        supervision::{DrainReason, SupervisorState},
    },
    shutdown::ShutdownMode,
};

use super::supervision::SupervisorRuntime;

impl SupervisorRuntime {
    pub(crate) async fn shutdown_all(&mut self) -> Result<SupervisorExit, SupervisorError> {
        self.state = SupervisorState::Stopping;
        self.send_event(SupervisorEvent::SupervisorStopping);
        self.cancel_running_children();
        self.drain_children(true).await?;
        self.send_event(SupervisorEvent::SupervisorStopped);
        Ok(SupervisorExit::Shutdown)
    }

    pub(crate) async fn drain_for_group_restart(&mut self) -> Result<(), SupervisorError> {
        self.cancel_running_children();
        self.drain_children(false).await
    }

    fn cancel_running_children(&mut self) {
        for child in self.children.values_mut() {
            if matches!(
                child.state,
                RuntimeChildState::Running | RuntimeChildState::Starting
            ) {
                child.state = RuntimeChildState::Stopping;
            }
        }
        // Child tokens are children of group_token, so this cancels all of them.
        self.group_token.cancel();
    }

    async fn drain_children(&mut self, stopping_supervisor: bool) -> Result<(), SupervisorError> {
        let mut abort_now = Vec::new();
        let mut max_grace = None;

        for (id, child) in &self.children {
            if !matches!(
                child.state,
                RuntimeChildState::Running
                    | RuntimeChildState::Starting
                    | RuntimeChildState::Stopping
            ) {
                continue;
            }

            match child.spec.shutdown_policy.mode {
                ShutdownMode::Abort => abort_now.push(id.clone()),
                ShutdownMode::Cooperative | ShutdownMode::CooperativeThenAbort => {
                    max_grace = Some(max_grace.map_or(
                        child.spec.shutdown_policy.grace,
                        |current: std::time::Duration| {
                            current.max(child.spec.shutdown_policy.grace)
                        },
                    ));
                }
            }
        }

        for id in abort_now {
            self.abort_child(&id);
        }

        if let Some(grace) = max_grace {
            let deadline = Instant::now() + grace;
            loop {
                if self.join_set.is_empty() {
                    break;
                }

                tokio::select! {
                    maybe = self.join_set.join_next_with_id() => {
                        let Some(joined) = maybe else {
                            break;
                        };
                        self.handle_drained_join(
                            joined,
                            if stopping_supervisor {
                                DrainReason::Shutdown
                            } else {
                                DrainReason::GroupRestart
                            },
                        )?;
                    }
                    _ = sleep_until(deadline) => {
                        break;
                    }
                }
            }
        }

        let cooperative_timeouts = self.remaining_cooperative_ids();
        if !cooperative_timeouts.is_empty() {
            let ids = cooperative_timeouts.join(", ");
            for id in &cooperative_timeouts {
                self.abort_child(id);
            }
            self.drain_join_set().await?;
            return Err(SupervisorError::ShutdownTimedOut(ids));
        }

        self.abort_children_requiring_abort();
        self.drain_join_set().await
    }

    fn abort_children_requiring_abort(&mut self) {
        let ids: Vec<String> = self
            .children
            .iter()
            .filter(|(_, child)| {
                matches!(
                    child.state,
                    RuntimeChildState::Running
                        | RuntimeChildState::Starting
                        | RuntimeChildState::Stopping
                ) && matches!(
                    child.spec.shutdown_policy.mode,
                    ShutdownMode::CooperativeThenAbort | ShutdownMode::Abort
                )
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in ids {
            self.abort_child(&id);
        }
    }

    fn remaining_cooperative_ids(&self) -> Vec<String> {
        self.children
            .iter()
            .filter(|(_, child)| {
                matches!(
                    child.state,
                    RuntimeChildState::Running
                        | RuntimeChildState::Starting
                        | RuntimeChildState::Stopping
                ) && matches!(child.spec.shutdown_policy.mode, ShutdownMode::Cooperative)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn abort_child(&mut self, id: &str) {
        if let Some(child) = self.children.get(id)
            && let Some(abort_handle) = child.abort_handle.as_ref()
        {
            abort_handle.abort();
        }
    }

    async fn drain_join_set(&mut self) -> Result<(), SupervisorError> {
        while let Some(joined) = self.join_set.join_next_with_id().await {
            self.handle_drained_join(joined, DrainReason::Shutdown)?;
        }
        Ok(())
    }
}
