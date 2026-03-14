use thiserror::Error;

/// Errors returned when building a [`Supervisor`](crate::Supervisor) from a
/// [`SupervisorBuilder`](crate::SupervisorBuilder).
#[derive(Debug, Error, Eq, PartialEq)]
pub enum BuildError {
    /// Two or more children share the same id string.
    #[error("duplicate child id: {0}")]
    DuplicateChildId(String),
    /// The builder has no children. A supervisor must have at least one child.
    #[error("supervisor requires at least one child")]
    EmptyChildren,
    /// A configuration value (channel capacity, restart intensity, etc.) is
    /// invalid.
    #[error("invalid supervisor configuration: {0}")]
    InvalidConfig(&'static str),
}

/// Fatal errors that cause a running supervisor to exit.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SupervisorError {
    /// A child exceeded its [`RestartIntensity`](crate::RestartIntensity)
    /// limit, so the supervisor cannot continue.
    #[error("restart intensity exceeded")]
    RestartIntensityExceeded,
    /// One or more children did not exit within their configured grace period
    /// during shutdown. The contained string lists the timed-out child ids.
    #[error("shutdown timed out: {0}")]
    ShutdownTimedOut(String),
    /// An unexpected internal condition. Indicates a bug in the supervisor
    /// runtime.
    #[error("internal supervisor error: {0}")]
    Internal(String),
}

/// Errors returned by control-plane operations on a
/// [`SupervisorHandle`](crate::SupervisorHandle) (e.g. adding or removing
/// children at runtime).
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ControlError {
    /// The control channel is full. The caller may retry after the supervisor
    /// has had time to drain queued commands.
    #[error("supervisor control plane is busy")]
    Busy,
    /// A child with this id already exists in the supervisor.
    #[error("duplicate child id: {0}")]
    DuplicateChildId(String),
    /// No child with this id is known to the supervisor.
    #[error("unknown child id: {0}")]
    UnknownChildId(String),
    /// A removal request for this child is already in progress.
    #[error("child removal already in progress: {0}")]
    ChildRemovalInProgress(String),
    /// The child spec contains invalid configuration.
    #[error("invalid child configuration: {0}")]
    InvalidConfig(&'static str),
    /// Cannot remove the last active child. A supervisor must always have at
    /// least one child.
    #[error("cannot remove the last active child")]
    LastChildRemovalUnsupported,
    /// The supervisor is in the process of shutting down and is no longer
    /// accepting commands.
    #[error("supervisor is stopping")]
    SupervisorStopping,
    /// A child did not exit within its grace period during removal.
    #[error("child removal timed out: {0}")]
    ShutdownTimedOut(String),
    /// The supervisor task has already exited and the control channel is
    /// closed.
    #[error("supervisor control plane is unavailable")]
    Unavailable,
    /// An unexpected internal condition. Indicates a bug in the supervisor
    /// runtime.
    #[error("internal supervisor control error: {0}")]
    Internal(String),
}

/// The final exit reason of a supervisor.
///
/// Returned by [`SupervisorHandle::wait`](crate::SupervisorHandle::wait) on
/// success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SupervisorExit {
    /// The supervisor was explicitly shut down via
    /// [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown).
    Shutdown,
    /// All children exited cleanly with no failures.
    Completed,
    /// At least one child exited with a failure and was not restarted (e.g. a
    /// [`Temporary`](crate::Restart::Temporary) child that returned an error).
    /// Natural completion is based on each child's latest terminal status, so
    /// failures from superseded generations do not poison a later clean exit.
    Failed,
}

impl std::fmt::Display for SupervisorExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shutdown => f.write_str("shutdown"),
            Self::Completed => f.write_str("completed"),
            Self::Failed => f.write_str("failed"),
        }
    }
}
