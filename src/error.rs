use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BuildError {
    #[error("duplicate child id: {0}")]
    DuplicateChildId(String),
    #[error("supervisor requires at least one child")]
    EmptyChildren,
    #[error("invalid supervisor configuration: {0}")]
    InvalidConfig(&'static str),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SupervisorError {
    #[error("restart intensity exceeded")]
    RestartIntensityExceeded,
    #[error("shutdown timed out: {0}")]
    ShutdownTimedOut(String),
    #[error("internal supervisor error: {0}")]
    Internal(String),
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ControlError {
    #[error("supervisor control plane is busy")]
    Busy,
    #[error("duplicate child id: {0}")]
    DuplicateChildId(String),
    #[error("unknown child id: {0}")]
    UnknownChildId(String),
    #[error("child removal already in progress: {0}")]
    ChildRemovalInProgress(String),
    #[error("invalid child configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("cannot remove the last active child")]
    LastChildRemovalUnsupported,
    #[error("supervisor is stopping")]
    SupervisorStopping,
    #[error("child removal timed out: {0}")]
    ShutdownTimedOut(String),
    #[error("supervisor control plane is unavailable")]
    Unavailable,
    #[error("internal supervisor control error: {0}")]
    Internal(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorExit {
    Shutdown,
    Completed,
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
