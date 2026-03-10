use std::time::Duration;

/// How the supervisor stops a child task during shutdown or removal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownMode {
    /// Wait for the grace period, then report a timeout if the task has not exited.
    Cooperative,
    /// Wait for the grace period, then issue a Tokio abort and return promptly.
    ///
    /// Abort remains cooperative at Tokio poll boundaries, so a non-yielding
    /// future can outlive the shutdown call briefly. For hard-stop guarantees,
    /// isolate blocking work outside the supervised Tokio task.
    CooperativeThenAbort,
    /// Issue a Tokio abort and return promptly.
    ///
    /// Abort remains cooperative at Tokio poll boundaries, so this mode does not
    /// forcibly preempt a non-yielding future.
    Abort,
}

/// Shutdown behaviour for a single child, combining a [`ShutdownMode`] with a
/// grace period.
///
/// The default is [`CooperativeThenAbort`](ShutdownMode::CooperativeThenAbort)
/// with a 5-second grace period.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownPolicy {
    /// How long to wait for the child to exit after its cancellation token is
    /// triggered.
    pub grace: Duration,
    /// What to do when the grace period expires (or immediately, for
    /// [`Abort`](ShutdownMode::Abort)).
    pub mode: ShutdownMode,
}

impl ShutdownPolicy {
    /// Creates a policy with an explicit mode and grace period.
    pub fn new(grace: Duration, mode: ShutdownMode) -> Self {
        Self { grace, mode }
    }

    /// Cooperative shutdown: cancel the child and wait up to `grace` for it to
    /// exit. If the child does not exit within the grace period, a timeout
    /// error is reported but the task is **not** aborted.
    pub fn cooperative(grace: Duration) -> Self {
        Self::new(grace, ShutdownMode::Cooperative)
    }

    /// Cancel the child and wait up to `grace`; if it has not exited by then,
    /// abort the Tokio task.
    pub fn cooperative_then_abort(grace: Duration) -> Self {
        Self::new(grace, ShutdownMode::CooperativeThenAbort)
    }

    /// Abort the Tokio task immediately with no grace period.
    pub fn abort() -> Self {
        Self::new(Duration::ZERO, ShutdownMode::Abort)
    }
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self::cooperative_then_abort(Duration::from_secs(5))
    }
}
