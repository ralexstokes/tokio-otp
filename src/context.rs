use tokio_util::sync::CancellationToken;

/// Runtime context passed to a child function on each (re)start.
///
/// The child should select on [`token`](Self::token) to detect when the
/// supervisor asks it to stop. The [`supervisor_token`](Self::supervisor_token)
/// provides a read-only view of the parent supervisor's cancellation state.
#[derive(Clone, Debug)]
pub struct ChildContext {
    /// The child's unique identifier within its supervisor.
    pub id: String,
    /// Monotonically increasing counter that distinguishes successive
    /// incarnations of the same child spec. Starts at 0 for the first spawn.
    pub generation: u64,
    /// Cancellation token for this specific child instance. The supervisor
    /// cancels this token when the child should stop (shutdown, removal, or
    /// group restart).
    pub token: CancellationToken,
    /// Read-only view of the supervisor's own cancellation state.
    pub supervisor_token: SupervisorToken,
}

/// Read-only view of the supervisor's cancellation token.
///
/// Children can observe supervisor-level cancellation but cannot trigger it.
#[derive(Clone, Debug)]
pub struct SupervisorToken(CancellationToken);

impl SupervisorToken {
    pub(crate) fn new(token: CancellationToken) -> Self {
        Self(token)
    }

    /// Returns a future that completes when the supervisor is cancelled.
    pub async fn cancelled(&self) {
        self.0.cancelled().await;
    }

    /// Returns `true` if the supervisor has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}
