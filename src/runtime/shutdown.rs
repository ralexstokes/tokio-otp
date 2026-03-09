use tokio::time::{Instant, sleep_until};

use crate::{
    error::{SupervisorError, SupervisorExit},
    event::SupervisorEvent,
    runtime::{
        child_runtime::{ChildRuntime, RuntimeChildState},
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
        self.drain_children(DrainReason::Shutdown).await?;
        self.send_event(SupervisorEvent::SupervisorStopped);
        Ok(SupervisorExit::Shutdown)
    }

    pub(crate) async fn drain_for_group_restart(&mut self) -> Result<(), SupervisorError> {
        self.cancel_running_children();
        self.drain_children(DrainReason::GroupRestart).await
    }

    fn cancel_running_children(&mut self) {
        for child in self.children.iter_mut().rev() {
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

    async fn drain_children(&mut self, reason: DrainReason) -> Result<(), SupervisorError> {
        let mut max_grace: Option<std::time::Duration> = None;

        for child in &self.children {
            if !child.state.is_active() {
                continue;
            }

            let grace = child.spec.shutdown_policy.grace;
            match child.spec.shutdown_policy.mode {
                ShutdownMode::Abort => {}
                ShutdownMode::Cooperative | ShutdownMode::CooperativeThenAbort => {
                    max_grace = Some(max_grace.map_or(grace, |current| current.max(grace)));
                }
            }
        }

        abort_matching_children(&self.children, |child| {
            matches!(child.spec.shutdown_policy.mode, ShutdownMode::Abort)
        });

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
                        self.handle_drained_join(joined, reason)?;
                    }
                    _ = sleep_until(deadline) => {
                        break;
                    }
                }
            }
        }

        let timed_out = cooperative_timeout_names(&self.children, &self.child_names);
        if !timed_out.is_empty() {
            abort_matching_children(&self.children, |child| {
                matches!(child.spec.shutdown_policy.mode, ShutdownMode::Cooperative)
                    || matches!(
                        child.spec.shutdown_policy.mode,
                        ShutdownMode::CooperativeThenAbort | ShutdownMode::Abort
                    )
            });
            self.drain_join_set(reason).await?;
            return Err(SupervisorError::ShutdownTimedOut(timed_out));
        }

        abort_matching_children(&self.children, |child| {
            matches!(
                child.spec.shutdown_policy.mode,
                ShutdownMode::CooperativeThenAbort | ShutdownMode::Abort
            )
        });
        self.drain_join_set(reason).await
    }

    async fn drain_join_set(&mut self, reason: DrainReason) -> Result<(), SupervisorError> {
        while let Some(joined) = self.join_set.join_next_with_id().await {
            self.handle_drained_join(joined, reason)?;
        }
        Ok(())
    }
}

fn abort_matching_children(children: &[ChildRuntime], predicate: impl Fn(&ChildRuntime) -> bool) {
    for child in children {
        if child.state.is_active()
            && predicate(child)
            && let Some(abort_handle) = child.abort_handle.as_ref()
        {
            abort_handle.abort();
        }
    }
}

fn cooperative_timeout_names(children: &[ChildRuntime], child_names: &[String]) -> String {
    let ids: Vec<&str> = children
        .iter()
        .zip(child_names)
        .filter(|(child, _)| {
            child.state.is_active()
                && matches!(child.spec.shutdown_policy.mode, ShutdownMode::Cooperative)
        })
        .map(|(_, name)| name.as_str())
        .collect();
    ids.join(", ")
}
