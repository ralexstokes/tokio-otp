use thiserror::Error;

/// A type-erased, thread-safe error type used by actor functions.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Errors detected while building a graph.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum BuildError {
    /// The same actor id was registered with an implementation twice.
    #[error("actor `{actor_id}` is registered more than once")]
    DuplicateActor {
        /// Actor id registered twice.
        actor_id: String,
    },
    /// An actor id was referenced with two different message types.
    #[error(
        "actor `{actor_id}` is used with message type `{requested}` but was first referenced with `{registered}`"
    )]
    MessageTypeMismatch {
        /// Actor id with conflicting message types.
        actor_id: String,
        /// Message type recorded when the actor was first referenced.
        registered: &'static str,
        /// Message type of the conflicting reference.
        requested: &'static str,
    },
    /// An actor id was declared but no implementation was registered.
    #[error("actor `{actor_id}` was declared but never registered")]
    MissingActor {
        /// Actor id without an implementation.
        actor_id: String,
    },
    /// The graph contains no actors.
    #[error("graph has no actors")]
    EmptyGraph,
}

/// Errors returned when sending to an actor mailbox.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SendError {
    /// The target actor has no bound mailbox right now.
    #[error("actor `{actor_id}` is not running")]
    ActorNotRunning {
        /// Target actor id.
        actor_id: String,
    },
    /// The target mailbox is at capacity.
    #[error("mailbox for actor `{actor_id}` is full")]
    MailboxFull {
        /// Target actor id.
        actor_id: String,
    },
    /// The target mailbox has been closed.
    #[error("mailbox for actor `{actor_id}` is closed")]
    MailboxClosed {
        /// Target actor id.
        actor_id: String,
    },
}

/// Errors returned by [`ActorRef::call`](crate::ActorRef::call).
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CallError {
    /// The request message could not be delivered.
    #[error(transparent)]
    Send(#[from] SendError),
    /// The actor dropped the [`Reply`](crate::Reply) without answering.
    #[error("actor `{actor_id}` dropped the reply")]
    ReplyDropped {
        /// Target actor id.
        actor_id: String,
    },
}

/// Errors returned by [`Graph::actor_ref`](crate::Graph::actor_ref).
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LookupError {
    /// No actor with the requested id exists in the graph.
    #[error("unknown actor `{actor_id}`")]
    UnknownActor {
        /// Requested actor id.
        actor_id: String,
    },
    /// The actor exists but has a different message type.
    #[error("actor `{actor_id}` has message type `{registered}`, not `{requested}`")]
    MessageTypeMismatch {
        /// Requested actor id.
        actor_id: String,
        /// Message type the actor was registered with.
        registered: &'static str,
        /// Message type requested by the caller.
        requested: &'static str,
    },
}

/// Errors terminating a graph run.
#[derive(Debug, Error)]
pub enum GraphError {
    /// An actor returned an error.
    #[error("actor `{actor_id}` failed: {source}")]
    ActorFailed {
        /// Failing actor id.
        actor_id: String,
        /// Error returned by the actor.
        #[source]
        source: BoxError,
    },
    /// An actor exited cleanly before shutdown was requested.
    #[error("actor `{actor_id}` exited before shutdown was requested")]
    ActorExitedEarly {
        /// Exiting actor id.
        actor_id: String,
    },
    /// An actor panicked.
    #[error("actor `{actor_id}` panicked")]
    ActorPanicked {
        /// Panicking actor id.
        actor_id: String,
    },
}
