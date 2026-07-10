use std::future::Future;

use crate::actor::context::ActorContext;

/// A type-erased, thread-safe error type used by actor functions.
///
/// This is identical to and interchangeable with `tokio_supervisor::BoxError`.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The result type returned by every actor function.
pub type ActorResult = Result<(), BoxError>;

/// Async actor interface with a typed mailbox.
///
/// [`Actor`](crate::Actor) is the recommended starting
/// point for ordinary actors: it provides the receive loop, lifecycle hooks,
/// and shutdown drain policy. Implement `RawActor` directly when an actor needs
/// custom loop control.
///
/// Implementors can use
/// `async fn run(&self, ctx: ActorContext<Self::Msg>) -> ActorResult` in their
/// trait impls. The actor value is cloned for each run, so a restart resets
/// state to the wiring-time value; per-incarnation resources are acquired
/// inside [`run`](Self::run).
///
/// This trait is deliberately not implemented for plain closures: an actor is
/// a named type that implements `RawActor`, which keeps the message type visible
/// at the definition site and the actor's state explicit.
pub trait RawActor: Clone + Send + Sync + 'static {
    /// The message type this actor receives.
    type Msg: Send + 'static;

    /// Runs the actor until it finishes or graph shutdown is requested.
    fn run(&self, ctx: ActorContext<Self::Msg>) -> impl Future<Output = ActorResult> + Send;
}
