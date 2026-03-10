use std::time::Duration;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShutdownPolicy {
    pub grace: Duration,
    pub mode: ShutdownMode,
}

impl ShutdownPolicy {
    pub fn new(grace: Duration, mode: ShutdownMode) -> Self {
        Self { grace, mode }
    }

    pub fn cooperative(grace: Duration) -> Self {
        Self::new(grace, ShutdownMode::Cooperative)
    }

    pub fn cooperative_then_abort(grace: Duration) -> Self {
        Self::new(grace, ShutdownMode::CooperativeThenAbort)
    }

    pub fn abort() -> Self {
        Self::new(Duration::ZERO, ShutdownMode::Abort)
    }
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self::cooperative_then_abort(Duration::from_secs(5))
    }
}
