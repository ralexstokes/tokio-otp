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
/// `async fn run(&mut self, ctx: ActorContext<Self::Msg>) -> ActorResult` in
/// their trait impls. Registration takes a reusable synchronous `Fn() -> A`
/// factory and invokes it once per initial start or supervised restart. Each
/// run therefore owns fresh incarnation-local state, including non-[`Clone`]
/// fields. Capture only durable configuration and shared handles that should
/// survive restarts in the factory; acquire fallible or asynchronous resources
/// at the start of [`run`](Self::run), where failure participates in
/// supervision and readiness.
///
/// This trait is deliberately not implemented for plain closures: an actor is
/// a named type that implements `RawActor`, which keeps the message type visible
/// at the definition site and the actor's state explicit.
pub trait RawActor: Send + Sync + 'static {
    /// The message type this actor receives.
    type Msg: Send + 'static;

    /// Returns whether this actor reports readiness explicitly from
    /// [`ActorContext::mark_ready`](crate::ActorContext::mark_ready).
    ///
    /// Handler-style [`Actor`](crate::Actor) implementations do this
    /// automatically after `on_start`; custom raw actors are ready immediately
    /// unless they override this method.
    fn readiness_gated(&self) -> bool {
        false
    }

    /// Runs the actor until it finishes or graph shutdown is requested.
    fn run(&mut self, ctx: ActorContext<Self::Msg>) -> impl Future<Output = ActorResult> + Send;
}
