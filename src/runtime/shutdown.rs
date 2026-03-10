use std::time::Instant as StdInstant;

use tokio::time::{Instant, sleep_until};
use tracing::{Instrument, info_span};

use crate::{
    error::{SupervisorError, SupervisorExit},
    event::SupervisorEvent,
    runtime::{
        child_runtime::RuntimeChildState,
        supervision::{ChildEntry, DrainReason, MembershipState, SupervisorState},
    },
    shutdown::ShutdownMode,
};

use super::supervision::SupervisorRuntime;

impl SupervisorRuntime {
    pub(crate) async fn shutdown_all(&mut self) -> Result<SupervisorExit, SupervisorError> {
        let span = info_span!(
            "shutdown",
            supervisor_name = %self.meta.observability.supervisor_name(),
            supervisor_path = %self.meta.observability.supervisor_path(),
        );

        async {
            self.state = SupervisorState::Stopping;
            self.cancel_running_children();
            self.send_event(SupervisorEvent::SupervisorStopping);
            self.drain_children(DrainReason::Shutdown).await?;
            self.finish_with_exit(SupervisorExit::Shutdown);
            Ok(SupervisorExit::Shutdown)
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn drain_for_group_restart(&mut self) -> Result<(), SupervisorError> {
        self.cancel_running_children();
        self.drain_children(DrainReason::GroupRestart).await
    }

    fn cancel_running_children(&mut self) {
        let mut cancelled = 0usize;
        for child in self.children.iter_mut().rev() {
            if child.membership == MembershipState::Removed {
                continue;
            }

            if matches!(
                child.runtime.state,
                RuntimeChildState::Running | RuntimeChildState::Starting
            ) {
                child.runtime.state = RuntimeChildState::Stopping;
                cancelled = cancelled.saturating_add(1);
            }
        }
        self.running_children = self.running_children.saturating_sub(cancelled);
        // Child tokens are children of group_token, so this cancels all of them.
        self.group_token.cancel();
    }

    async fn drain_children(&mut self, reason: DrainReason) -> Result<(), SupervisorError> {
        self.command_rx.close();
        let started_at = StdInstant::now();
        let mut max_grace: Option<std::time::Duration> = None;

        for child in &self.children {
            if child.membership == MembershipState::Removed || !child.runtime.state.is_active() {
                continue;
            }

            let grace = child.runtime.spec.shutdown_policy.grace;
            match child.runtime.spec.shutdown_policy.mode {
                ShutdownMode::Abort => {}
                ShutdownMode::Cooperative | ShutdownMode::CooperativeThenAbort => {
                    max_grace = Some(max_grace.map_or(grace, |current| current.max(grace)));
                }
            }
        }

        abort_matching_children(&self.children, |child| {
            matches!(child.runtime.spec.shutdown_policy.mode, ShutdownMode::Abort)
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

            if !self.join_set.is_empty() && matches!(reason, DrainReason::Shutdown) {
                self.meta
                    .observability
                    .record_shutdown_timeout("shutdown", None);
            }
        }

        let timed_out = cooperative_timeout_names(&self.children);
        if !timed_out.is_empty() {
            abort_matching_children(&self.children, |_| true);
            self.drain_join_set(reason).await?;
            self.meta.observability.record_shutdown_duration(
                shutdown_operation(reason),
                started_at.elapsed(),
                None,
            );
            return Err(SupervisorError::ShutdownTimedOut(timed_out));
        }

        abort_matching_children(&self.children, |child| {
            matches!(
                child.runtime.spec.shutdown_policy.mode,
                ShutdownMode::CooperativeThenAbort
            )
        });
        let result = self.drain_join_set(reason).await;
        self.meta.observability.record_shutdown_duration(
            shutdown_operation(reason),
            started_at.elapsed(),
            None,
        );
        result
    }

    async fn drain_join_set(&mut self, reason: DrainReason) -> Result<(), SupervisorError> {
        while let Some(joined) = self.join_set.join_next_with_id().await {
            self.handle_drained_join(joined, reason)?;
        }
        Ok(())
    }
}

fn abort_matching_children(children: &[ChildEntry], predicate: impl Fn(&ChildEntry) -> bool) {
    for child in children {
        if child.membership != MembershipState::Removed
            && child.runtime.state.is_active()
            && predicate(child)
            && let Some(abort_handle) = child.runtime.abort_handle.as_ref()
        {
            abort_handle.abort();
        }
    }
}

fn cooperative_timeout_names(children: &[ChildEntry]) -> String {
    let ids: Vec<&str> = children
        .iter()
        .filter(|child| {
            child.membership != MembershipState::Removed
                && child.runtime.state.is_active()
                && matches!(
                    child.runtime.spec.shutdown_policy.mode,
                    ShutdownMode::Cooperative
                )
        })
        .map(|child| child.id.as_str())
        .collect();
    ids.join(", ")
}

fn shutdown_operation(reason: DrainReason) -> &'static str {
    match reason {
        DrainReason::Shutdown => "shutdown",
        DrainReason::GroupRestart => "group_restart",
    }
}
