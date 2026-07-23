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
/// baseline captured at construction time. Explicitly readiness-gated
/// children become running only after they report readiness.
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

/// Reliable observer of a supervisor's cumulative restart activity, created by
/// [`SupervisorHandle::watch_restarts`](crate::SupervisorHandle::watch_restarts).
///
/// The watch tracks [`SupervisorSnapshot::total_restarts`] over the lossless
/// snapshot `watch` channel. The channel conflates intermediate snapshots but
/// never lags, and the counter is cumulative, so no restart is ever silently
/// missed: a batch of conflated updates is reported as a single delta covering
/// every restart in the batch. This makes it a sound control input for safety
/// mechanisms such as aggregate restart breakers — unlike counting
/// [`SupervisorEvent`](crate::SupervisorEvent)s from the lossy broadcast
/// channel.
///
/// The baseline is captured when the watch is created; restarts recorded
/// before that are not reported.
pub struct RestartWatch {
    observed: u64,
    snapshots: watch::Receiver<SupervisorSnapshot>,
}

impl RestartWatch {
    pub(crate) fn new(mut snapshots: watch::Receiver<SupervisorSnapshot>) -> Self {
        let observed = snapshots.borrow_and_update().total_restarts;
        Self {
            observed,
            snapshots,
        }
    }

    /// The cumulative restart count observed so far, including the baseline
    /// captured at creation.
    pub fn observed(&self) -> u64 {
        self.observed
    }

    /// Waits until the supervisor records further restarts and returns how
    /// many were recorded since the previous observation.
    ///
    /// Returns `None` once the supervisor has stopped and every recorded
    /// restart has been reported. If the watched supervisor is a nested child
    /// that is itself restarted by its parent, its counter restarts from zero;
    /// the watch resynchronizes its baseline and continues counting restarts
    /// of the new incarnation. (The nested supervisor's own restart is counted
    /// by the parent supervisor, not here.)
    pub async fn next(&mut self) -> Option<u64> {
        loop {
            if let Some(delta) = self.observe() {
                return Some(delta);
            }

            if self.snapshots.changed().await.is_err() {
                return self.observe();
            }
        }
    }

    fn observe(&mut self) -> Option<u64> {
        let total = self.snapshots.borrow_and_update().total_restarts;
        if total < self.observed {
            // A new incarnation of the supervisor reset the counter.
            self.observed = total;
            return None;
        }
        let delta = total - self.observed;
        self.observed = total;
        (delta > 0).then_some(delta)
    }
}

impl fmt::Debug for RestartWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestartWatch")
            .field("observed", &self.observed)
            .finish_non_exhaustive()
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
            total_restarts: 1,
            children: vec![crate::ChildSnapshot {
                id: "worker".to_owned(),
                generation: 1,
                started: true,
                startup_aborted: false,
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
