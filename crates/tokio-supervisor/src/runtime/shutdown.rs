use std::{collections::HashSet, time::Instant as StdInstant};

use slab::Slab;
use tokio::time::{Instant, sleep_until};
use tracing::{Instrument, info_span};

use crate::{
    error::SupervisorError,
    event::SupervisorEvent,
    runtime::{
        child_runtime::RuntimeChildState,
        supervision::{ChildEntry, DrainReason, MembershipState, SupervisorState},
    },
    shutdown::ShutdownMode,
};

use super::supervision::SupervisorRuntime;

impl SupervisorRuntime {
    pub(crate) async fn shutdown_all(&mut self) -> Result<(), SupervisorError> {
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
            self.finish();
            Ok(())
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn drain_for_group_restart(&mut self) -> Result<(), SupervisorError> {
        self.cancel_running_children();
        self.drain_children(DrainReason::GroupRestart).await
    }

    pub(crate) async fn drain_for_rest_for_one_restart(
        &mut self,
        keys: &[usize],
    ) -> Result<(), SupervisorError> {
        let keys: HashSet<_> = keys.iter().copied().collect();
        let mut cancelled = 0usize;
        for &key in self.child_order.iter().rev() {
            if !keys.contains(&key) {
                continue;
            }
            let Some(child) = self.children.get_mut(key) else {
                continue;
            };
            if matches!(
                child.runtime.state,
                RuntimeChildState::Running | RuntimeChildState::Starting
            ) {
                child.runtime.state = RuntimeChildState::Stopping;
                if let Some(token) = child.runtime.active_token.as_ref() {
                    token.cancel();
                }
                cancelled = cancelled.saturating_add(1);
            }
        }
        self.running_children = self.running_children.saturating_sub(cancelled);
        self.drain_child_subset(&keys).await
    }

    async fn drain_child_subset(&mut self, keys: &HashSet<usize>) -> Result<(), SupervisorError> {
        let started_at = StdInstant::now();
        let mut max_grace: Option<std::time::Duration> = None;
        for &key in keys {
            let Some(child) = self.children.get(key) else {
                continue;
            };
            if child.membership == MembershipState::Removed || !child.runtime.state.is_active() {
                continue;
            }
            let grace = child.runtime.definition.shutdown_policy.grace;
            if matches!(
                child.runtime.definition.shutdown_policy.mode,
                ShutdownMode::CooperativeStrict | ShutdownMode::CooperativeThenAbort
            ) {
                max_grace = Some(max_grace.map_or(grace, |current| current.max(grace)));
            }
        }

        abort_matching_children(&self.children, |key, child| {
            keys.contains(&key)
                && matches!(
                    child.runtime.definition.shutdown_policy.mode,
                    ShutdownMode::Abort
                )
        });
        tokio::task::yield_now().await;
        self.drain_ready_joins().await?;

        if let Some(grace) = max_grace {
            let deadline = Instant::now() + grace;
            while subset_has_active_tasks(&self.children, keys) {
                tokio::select! {
                    maybe = self.join_set.join_next_with_id() => {
                        let Some(joined) = maybe else { break; };
                        self.handle_drained_join(joined)?;
                    }
                    _ = sleep_until(deadline) => break,
                }
            }
        }

        let timed_out = collect_child_names(&self.children, |key, child| {
            keys.contains(&key)
                && child.membership != MembershipState::Removed
                && child.runtime.state.is_active()
                && matches!(
                    child.runtime.definition.shutdown_policy.mode,
                    ShutdownMode::CooperativeStrict
                )
        });
        if subset_has_active_tasks(&self.children, keys) {
            abort_matching_children(&self.children, |key, _| keys.contains(&key));
            tokio::task::yield_now().await;
            self.drain_ready_joins().await?;
        }
        let remaining = collect_child_names(&self.children, |key, child| {
            keys.contains(&key)
                && child.membership != MembershipState::Removed
                && child.runtime.state.is_active()
        });
        self.meta.observability.record_shutdown_duration(
            "group_restart",
            started_at.elapsed(),
            None,
        );
        if !timed_out.is_empty() {
            return Err(SupervisorError::ShutdownTimedOut(timed_out));
        }
        if !remaining.is_empty() {
            return Err(SupervisorError::ShutdownTimedOut(remaining));
        }
        Ok(())
    }

    fn cancel_running_children(&mut self) {
        let mut cancelled = 0usize;
        for &key in self.child_order.iter().rev() {
            let Some(child) = self.children.get_mut(key) else {
                continue;
            };

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
        if matches!(reason, DrainReason::Shutdown) {
            self.command_rx.close();
        }
        let started_at = StdInstant::now();
        let mut max_grace: Option<std::time::Duration> = None;

        // Use a single deadline for the whole drain equal to the maximum grace
        // among cooperative children. This keeps shutdown and `OneForAll`
        // restarts from compounding per-child grace periods serially.
        for (_, child) in self.children.iter() {
            if child.membership == MembershipState::Removed || !child.runtime.state.is_active() {
                continue;
            }

            let grace = child.runtime.definition.shutdown_policy.grace;
            match child.runtime.definition.shutdown_policy.mode {
                ShutdownMode::Abort => {}
                ShutdownMode::CooperativeStrict | ShutdownMode::CooperativeThenAbort => {
                    max_grace = Some(max_grace.map_or(grace, |current| current.max(grace)));
                }
            }
        }

        abort_matching_children(&self.children, |_, child| {
            matches!(
                child.runtime.definition.shutdown_policy.mode,
                ShutdownMode::Abort
            )
        });
        tokio::task::yield_now().await;
        self.drain_ready_joins().await?;
        if self.live_tasks == 0 {
            self.meta.observability.record_shutdown_duration(
                shutdown_operation(reason),
                started_at.elapsed(),
                None,
            );
            return Ok(());
        }

        if let Some(grace) = max_grace {
            let deadline = Instant::now() + grace;
            loop {
                if self.live_tasks == 0 {
                    break;
                }

                tokio::select! {
                    maybe = self.join_set.join_next_with_id() => {
                        let Some(joined) = maybe else {
                            break;
                        };
                        self.handle_drained_join(joined)?;
                    }
                    _ = sleep_until(deadline) => {
                        break;
                    }
                }
            }

            if self.live_tasks != 0 && matches!(reason, DrainReason::Shutdown) {
                self.meta
                    .observability
                    .record_shutdown_timeout("shutdown", None);
            }
        }

        let timed_out = cooperative_timeout_names(&self.children);
        let remaining = active_task_names(&self.children);
        if !remaining.is_empty() {
            abort_matching_children(&self.children, |_, _| true);
            tokio::task::yield_now().await;
            self.drain_ready_joins().await?;
        }

        let remaining = active_task_names(&self.children);
        if !timed_out.is_empty() {
            self.meta.observability.record_shutdown_duration(
                shutdown_operation(reason),
                started_at.elapsed(),
                None,
            );
            return Err(SupervisorError::ShutdownTimedOut(timed_out));
        }

        if !remaining.is_empty() {
            self.meta.observability.record_shutdown_duration(
                shutdown_operation(reason),
                started_at.elapsed(),
                None,
            );
            return match reason {
                DrainReason::Shutdown => Ok(()),
                DrainReason::GroupRestart => Err(SupervisorError::ShutdownTimedOut(remaining)),
            };
        }

        self.meta.observability.record_shutdown_duration(
            shutdown_operation(reason),
            started_at.elapsed(),
            None,
        );
        Ok(())
    }
}

fn abort_matching_children(
    children: &Slab<ChildEntry>,
    predicate: impl Fn(usize, &ChildEntry) -> bool,
) {
    for (key, child) in children.iter() {
        if child.membership != MembershipState::Removed
            && child.runtime.state.is_active()
            && predicate(key, child)
            && let Some(abort_handle) = child.runtime.abort_handle.as_ref()
        {
            abort_handle.abort();
        }
    }
}

fn cooperative_timeout_names(children: &Slab<ChildEntry>) -> String {
    collect_child_names(children, |_, child| {
        child.membership != MembershipState::Removed
            && child.runtime.state.is_active()
            && matches!(
                child.runtime.definition.shutdown_policy.mode,
                ShutdownMode::CooperativeStrict
            )
    })
}

fn active_task_names(children: &Slab<ChildEntry>) -> String {
    collect_child_names(children, |_, child| {
        child.membership != MembershipState::Removed && child.runtime.state.is_active()
    })
}

fn collect_child_names(
    children: &Slab<ChildEntry>,
    predicate: impl Fn(usize, &ChildEntry) -> bool,
) -> String {
    children
        .iter()
        .filter(|(key, child)| predicate(*key, child))
        .map(|(_, child)| child)
        .map(|child| child.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn subset_has_active_tasks(children: &Slab<ChildEntry>, keys: &HashSet<usize>) -> bool {
    keys.iter().any(|&key| {
        children.get(key).is_some_and(|child| {
            child.membership != MembershipState::Removed && child.runtime.state.is_active()
        })
    })
}

fn shutdown_operation(reason: DrainReason) -> &'static str {
    match reason {
        DrainReason::Shutdown => "shutdown",
        DrainReason::GroupRestart => "group_restart",
    }
}
