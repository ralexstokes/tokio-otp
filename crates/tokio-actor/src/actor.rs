use std::future::Future;

use crate::context::ActorContext;

/// A type-erased, thread-safe error type used by actor functions.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The result type returned by every actor function.
pub type ActorResult = Result<(), BoxError>;

/// Async actor interface with a typed mailbox.
///
/// Implementors can use
/// `async fn run(&self, ctx: ActorContext<Self::Msg>) -> ActorResult` in their
/// trait impls. The actor value is cloned for each graph run.
///
/// This trait is deliberately not implemented for plain closures: an actor is
/// a named type that implements `Actor`, which keeps the message type visible
/// at the definition site and the actor's state explicit.
pub trait Actor: Clone + Send + Sync + 'static {
    /// The message type this actor receives.
    type Msg: Send + 'static;

    /// Runs the actor until it finishes or graph shutdown is requested.
    fn run(&self, ctx: ActorContext<Self::Msg>) -> impl Future<Output = ActorResult> + Send;
}
