use thiserror::Error;
use tokio_actor::{GraphError, LookupError, RegistryError};
use tokio_supervisor::ControlError;

/// Errors returned while adapting actor graphs into supervisor children.
#[derive(Debug, Error)]
pub enum BuildError {
    /// The graph could not be decomposed into an actor set.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// Actor registry initialization failed.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The supplied supervisor configuration was invalid.
    #[error(transparent)]
    Supervisor(#[from] tokio_supervisor::BuildError),
    /// An override referenced an actor id that does not exist in the graph.
    #[error("unknown actor `{actor_id}`")]
    UnknownActor { actor_id: String },
}

/// Errors returned by runtime dynamic actor operations.
#[derive(Debug, Error)]
pub enum DynamicActorError {
    /// Dynamic actor support is unavailable for this runtime.
    #[error("dynamic actor support is unavailable for this runtime")]
    Unsupported,
    /// A runtime actor registry operation failed.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// A typed actor lookup failed.
    #[error(transparent)]
    Lookup(#[from] LookupError),
    /// The underlying supervisor rejected the control request.
    #[error(transparent)]
    Control(#[from] ControlError),
}
