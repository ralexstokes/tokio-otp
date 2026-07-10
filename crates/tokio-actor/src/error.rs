use thiserror::Error;

/// Errors returned while validating a graph during build.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum GraphBuildError {
    /// The graph was built without any actors.
    #[error("graph must contain at least one actor")]
    EmptyGraph,
    /// Two actor implementations shared the same id.
    #[error("duplicate actor id `{actor_id}`")]
    DuplicateActorId {
        /// Actor id registered twice.
        actor_id: String,
    },
    /// An actor slot was opened but no implementation was registered.
    #[error("actor `{actor_id}` slot was opened but never filled")]
    MissingActor {
        /// Actor id without an implementation.
        actor_id: String,
    },
    /// Generic invalid builder configuration.
    #[error("{0}")]
    InvalidConfig(&'static str),
}

/// Errors returned when sending to an actor mailbox.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum SendError {
    /// The target actor is currently unbound because it is not running.
    #[error("actor `{actor_id}` is not currently running")]
    ActorNotRunning { actor_id: String },
    /// The target actor has terminated and no restart is scheduled.
    #[error("actor `{actor_id}` has terminated")]
    ActorTerminated { actor_id: String },
    /// The target actor's mailbox is full.
    #[error("mailbox for actor `{actor_id}` is full")]
    MailboxFull { actor_id: String },
    /// The target actor's mailbox is closed.
    #[error("mailbox for actor `{actor_id}` is closed")]
    MailboxClosed { actor_id: String },
}

/// Errors returned by [`ActorRef::call`](crate::ActorRef::call).
#[derive(Debug, Error, Clone, Eq, PartialEq)]
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

/// Errors returned by typed actor lookups.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum LookupError {
    /// No actor with the requested id exists.
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
