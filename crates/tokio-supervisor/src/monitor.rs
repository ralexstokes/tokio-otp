use std::{
    fmt,
    future::{Future, IntoFuture},
    pin::Pin,
};

use tokio::sync::watch;

use crate::{
    error::RestartMonitorError,
    snapshot::{ChildMembershipView, ChildStateView, SupervisorSnapshot, SupervisorStateView},
};

/// Awaitable created by [`SupervisorHandle::monitor_restart`](crate::SupervisorHandle::monitor_restart).
///
/// The monitor resolves when the watched direct child is next observed
/// [`Running`](ChildStateView::Running) with a generation greater than the
/// baseline captured at construction time. The supervisor marks a child
/// running when its task is spawned; actor-level readiness hooks may still be
/// running at that point.
pub struct RestartMonitor {
    id: String,
    baseline_generation: u64,
    snapshots: watch::Receiver<SupervisorSnapshot>,
}

impl RestartMonitor {
    pub(crate) fn new(
        id: String,
        baseline_generation: u64,
        snapshots: watch::Receiver<SupervisorSnapshot>,
    ) -> Self {
        Self {
            id,
            baseline_generation,
            snapshots,
        }
    }

    /// The child id this monitor watches.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The generation observed when the monitor was created.
    pub fn baseline_generation(&self) -> u64 {
        self.baseline_generation
    }

    async fn wait(mut self) -> Result<u64, RestartMonitorError> {
        loop {
            let observation = {
                let snapshot = self.snapshots.borrow();
                observe_snapshot(&snapshot, &self.id, self.baseline_generation)
            };

            match observation {
                RestartObservation::Resolved(generation) => return Ok(generation),
                RestartObservation::Failed(error) => return Err(error),
                RestartObservation::Pending => {}
            }

            self.snapshots
                .changed()
                .await
                .map_err(|_| RestartMonitorError::SupervisorStopped)?;
        }
    }
}

impl fmt::Debug for RestartMonitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestartMonitor")
            .field("id", &self.id)
            .field("baseline_generation", &self.baseline_generation)
            .finish_non_exhaustive()
    }
}

impl IntoFuture for RestartMonitor {
    type Output = Result<u64, RestartMonitorError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'static>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.wait())
    }
}

enum RestartObservation {
    Resolved(u64),
    Failed(RestartMonitorError),
    Pending,
}

fn observe_snapshot(
    snapshot: &SupervisorSnapshot,
    id: &str,
    baseline_generation: u64,
) -> RestartObservation {
    let Some(child) = snapshot.child(id) else {
        return RestartObservation::Failed(RestartMonitorError::ChildRemoved(id.to_owned()));
    };

    if child.generation > baseline_generation && child.state == ChildStateView::Running {
        return RestartObservation::Resolved(child.generation);
    }

    if child.membership == ChildMembershipView::Removing {
        return RestartObservation::Failed(RestartMonitorError::ChildRemoved(id.to_owned()));
    }

    if snapshot.state == SupervisorStateView::Stopped {
        return RestartObservation::Failed(RestartMonitorError::SupervisorStopped);
    }

    RestartObservation::Pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::Strategy;

    #[test]
    fn resolved_generation_wins_over_terminal_snapshot_state() {
        let snapshot = SupervisorSnapshot {
            state: SupervisorStateView::Stopped,
            strategy: Strategy::OneForOne,
            children: vec![crate::ChildSnapshot {
                id: "worker".to_owned(),
                generation: 1,
                state: ChildStateView::Running,
                membership: ChildMembershipView::Removing,
                last_exit: None,
                restart_count: 1,
                next_restart_in: None,
                supervisor: None,
            }],
        };

        match observe_snapshot(&snapshot, "worker", 0) {
            RestartObservation::Resolved(1) => {}
            RestartObservation::Resolved(generation) => {
                panic!("resolved with unexpected generation {generation}");
            }
            RestartObservation::Failed(error) => {
                panic!("terminal state won over resolution: {error}");
            }
            RestartObservation::Pending => panic!("snapshot should have resolved"),
        }
    }
}
