use crate::actor::raw::RawActor;

/// A reusable recipe for constructing one actor incarnation.
///
/// [`build`](Self::build) is invoked exactly once for the initial start and
/// once for every supervised restart. It runs inside the supervised actor
/// future, so construction panics follow the same binding, monitoring, and
/// supervision path as startup and run panics.
///
/// Factory fields and closure captures are durable configuration that survives
/// restarts. The returned actor owns fresh incarnation-local state, which does
/// not need to implement [`Clone`]. Fallible or asynchronous resource
/// acquisition belongs in [`Actor::on_start`](crate::Actor::on_start) (the OTP
/// `init` idiom), where failure participates in supervision and readiness.
///
/// Any zero-argument constructor path, such as `Worker::new` or
/// `Worker::default`, already implements this trait through the blanket
/// implementation for closures and functions; no `Default`-specific factory
/// machinery is needed. A direct blanket implementation for every
/// `RawActor + Default` actor would overlap this closure blanket and is not
/// permitted by Rust's coherence rules (E0119).
pub trait ActorFactory: Send + Sync + 'static {
    /// The actor constructed for each incarnation.
    type Actor: RawActor;

    /// Constructs fresh incarnation-local actor state.
    fn build(&self) -> Self::Actor;
}

impl<A, F> ActorFactory for F
where
    A: RawActor,
    F: Fn() -> A + Send + Sync + 'static,
{
    type Actor = A;

    fn build(&self) -> Self::Actor {
        self()
    }
}
