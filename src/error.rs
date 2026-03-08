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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorExit {
    Shutdown,
    Completed,
    Failed,
}
