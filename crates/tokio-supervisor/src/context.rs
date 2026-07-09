use tokio_util::sync::CancellationToken;

/// Runtime context passed to a child function on each (re)start.
///
/// The child should select on [`shutdown_token`](Self::shutdown_token) to
/// detect when the supervisor asks it to stop. The
/// [`supervisor_token`](Self::supervisor_token) provides a read-only view of
/// the parent supervisor's cancellation state.
#[derive(Clone, Debug)]
pub struct ChildContext {
    id: String,
    generation: u64,
    token: CancellationToken,
    supervisor: SupervisorToken,
}

impl ChildContext {
    pub(crate) fn new(
        id: String,
        generation: u64,
        token: CancellationToken,
        supervisor: SupervisorToken,
    ) -> Self {
        Self {
            id,
            generation,
            token,
            supervisor,
        }
    }

    /// Returns the child's unique identifier within its supervisor.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the incarnation counter (0 for the first spawn).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the cancellation token for this specific child instance.
    ///
    /// The supervisor cancels it when the child should stop.
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.token
    }

    /// Returns a read-only view of the supervisor's cancellation state.
    pub fn supervisor_token(&self) -> &SupervisorToken {
        &self.supervisor
    }
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
