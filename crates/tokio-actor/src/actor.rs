use std::{future::Future, pin::Pin, sync::Arc};

use crate::context::ActorContext;

/// A type-erased, thread-safe error type used by actor functions.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The result type returned by every actor function.
pub type ActorResult = Result<(), BoxError>;

pub(crate) type ActorFuture = Pin<Box<dyn Future<Output = ActorResult> + Send + 'static>>;

/// Async actor interface for reusable actor implementations.
///
/// Implementors can use `async fn run(&self, ctx: ActorContext) -> ActorResult`
/// in their trait impls. Async closures that implement
/// `Fn(ActorContext) -> impl Future<Output = ActorResult>` also implement
/// [`Actor`], so [`ActorSpec::from_actor`] is the only constructor needed for
/// both named actor types and inline closure actors.
pub trait Actor: Clone + Send + Sync + 'static {
    /// Runs the actor until it finishes or graph shutdown is requested.
    fn run(&self, ctx: ActorContext) -> impl Future<Output = ActorResult> + Send;
}

impl<F, Fut> Actor for F
where
    F: Fn(ActorContext) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ActorResult> + Send,
{
    fn run(&self, ctx: ActorContext) -> impl Future<Output = ActorResult> + Send {
        (self)(ctx)
    }
}

/// Specification for an actor within a graph.
///
/// An `ActorSpec` stores an actor id plus a factory that builds a fresh actor
/// future each time the enclosing graph is run. The factory receives a fresh
/// [`ActorContext`] containing the actor's mailbox, linked peers, and the
/// shared shutdown token.
#[derive(Clone)]
pub struct ActorSpec {
    pub(crate) inner: Arc<ActorSpecInner>,
}

#[derive(Clone)]
pub(crate) struct ActorSpecInner {
    pub(crate) id: String,
    pub(crate) factory: Arc<dyn ActorFactory>,
}

pub(crate) trait ActorFactory: Send + Sync + 'static {
    fn make(&self, ctx: ActorContext) -> ActorFuture;
}

struct InstanceFactory<A> {
    actor: A,
}

impl<A> ActorFactory for InstanceFactory<A>
where
    A: Actor,
{
    fn make(&self, ctx: ActorContext) -> ActorFuture {
        let actor = self.actor.clone();
        Box::pin(async move { actor.run(ctx).await })
    }
}

impl ActorSpec {
    /// Creates a new actor specification from an [`Actor`] implementation.
    ///
    /// The actor value is cloned for each graph run before [`Actor::run`] is
    /// invoked. Inline async closures work here too because they implement
    /// [`Actor`] automatically when they are cloneable.
    pub fn from_actor<A>(id: impl Into<String>, actor: A) -> Self
    where
        A: Actor,
    {
        Self {
            inner: Arc::new(ActorSpecInner {
                id: id.into(),
                factory: Arc::new(InstanceFactory { actor }),
            }),
        }
    }

    /// Returns the actor's unique identifier.
    pub fn id(&self) -> &str {
        &self.inner.id
    }
}
