use thiserror::Error;

use crate::actor::BoxError;

/// Errors returned while validating a graph during build.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum BuildError {
    /// The graph was built without any actors.
    #[error("graph must contain at least one actor")]
    EmptyActors,
    /// Two actors shared the same id.
    #[error("duplicate actor id `{0}`")]
    DuplicateActorId(String),
    /// Two ingress points shared the same name.
    #[error("duplicate ingress name `{0}`")]
    DuplicateIngressName(String),
    /// A link referenced an actor id that was not defined.
    #[error("link source actor `{actor}` does not exist")]
    UnknownLinkSource { actor: String },
    /// A link target referenced an actor id that was not defined.
    #[error("link from `{from}` references unknown actor `{actor}`")]
    UnknownLinkTarget { from: String, actor: String },
    /// An ingress point referenced an actor id that was not defined.
    #[error("ingress `{ingress}` references unknown actor `{actor}`")]
    UnknownIngressTarget { ingress: String, actor: String },
    /// Generic invalid builder configuration.
    #[error("{0}")]
    InvalidConfig(&'static str),
}

/// Errors returned while running a graph.
#[derive(Debug, Error)]
pub enum GraphError {
    /// Another instance of the same graph spec is already running.
    #[error("graph is already running")]
    AlreadyRunning,
    /// The graph runtime observed invalid internal state.
    #[error("graph runtime state is invalid: {detail}")]
    InvalidState { detail: String },
    /// An actor returned `Ok(())` before graph shutdown was requested.
    #[error("actor `{actor_id}` exited before graph shutdown")]
    ActorStopped { actor_id: String },
    /// An actor returned an error.
    #[error("actor `{actor_id}` returned an error")]
    ActorFailed {
        actor_id: String,
        #[source]
        source: BoxError,
    },
    /// An actor panicked while running.
    #[error("actor `{actor_id}` panicked")]
    ActorPanicked { actor_id: String },
    /// An actor task was externally cancelled or aborted.
    #[error("actor `{actor_id}` was cancelled")]
    ActorCancelled { actor_id: String },
}

/// Errors returned when sending to an actor mailbox.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum SendError {
    /// The sender attempted to address a peer that is not linked.
    #[error("actor `{actor_id}` is not linked to peer `{peer_id}`")]
    UnknownPeer { actor_id: String, peer_id: String },
    /// The envelope exceeds the configured mailbox payload limit.
    #[error(
        "envelope for actor `{actor_id}` exceeds max size of {max_envelope_bytes} bytes ({envelope_len} bytes)"
    )]
    EnvelopeTooLarge {
        actor_id: String,
        envelope_len: usize,
        max_envelope_bytes: usize,
    },
    /// The target actor's mailbox is full.
    #[error("mailbox for actor `{actor_id}` is full")]
    MailboxFull { actor_id: String },
    /// The target actor's mailbox is closed.
    #[error("mailbox for actor `{actor_id}` is closed")]
    MailboxClosed { actor_id: String },
}

/// Errors returned by stable ingress handles.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum IngressError {
    /// The graph is not currently running, so the ingress is not bound.
    #[error("ingress `{ingress}` is not currently bound to actor `{actor_id}`")]
    NotRunning { ingress: String, actor_id: String },
    /// The ingress envelope exceeds the configured mailbox payload limit.
    #[error(
        "ingress `{ingress}` envelope for actor `{actor_id}` exceeds max size of {max_envelope_bytes} bytes ({envelope_len} bytes)"
    )]
    EnvelopeTooLarge {
        ingress: String,
        actor_id: String,
        envelope_len: usize,
        max_envelope_bytes: usize,
    },
    /// The ingress target mailbox is full.
    #[error("ingress `{ingress}` target actor `{actor_id}` mailbox is full")]
    MailboxFull { ingress: String, actor_id: String },
    /// The ingress target mailbox is closed.
    #[error("ingress `{ingress}` target actor `{actor_id}` mailbox is closed")]
    MailboxClosed { ingress: String, actor_id: String },
}
