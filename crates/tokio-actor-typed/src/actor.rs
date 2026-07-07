use std::{future::Future, marker::PhantomData};

use crate::{context::ActorContext, error::BoxError};

/// The result type returned by every actor function.
pub type ActorResult = Result<(), BoxError>;

/// Async actor interface with a typed mailbox.
///
/// Implementors can use
/// `async fn run(&self, ctx: ActorContext<Self::Msg>) -> ActorResult` in
/// their trait impls. The actor value is cloned for each graph run.
///
/// Closures cannot implement this trait through a blanket impl (the message
/// type parameter would be unconstrained); use
/// [`GraphBuilder::actor_fn`](crate::GraphBuilder::actor_fn) for inline
/// closure actors instead.
pub trait Actor: Clone + Send + Sync + 'static {
    /// The message type this actor receives.
    type Msg: Send + 'static;

    /// Runs the actor until it finishes or graph shutdown is requested.
    fn run(&self, ctx: ActorContext<Self::Msg>) -> impl Future<Output = ActorResult> + Send;
}

/// Adapter implementing [`Actor`] for closure actors.
pub(crate) struct FnActor<F, M> {
    f: F,
    _marker: PhantomData<fn(ActorContext<M>)>,
}

impl<F, M> FnActor<F, M> {
    pub(crate) fn new(f: F) -> Self {
        Self {
            f,
            _marker: PhantomData,
        }
    }
}

impl<F: Clone, M> Clone for FnActor<F, M> {
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            _marker: PhantomData,
        }
    }
}

impl<F, Fut, M> Actor for FnActor<F, M>
where
    F: Fn(ActorContext<M>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ActorResult> + Send,
    M: Send + 'static,
{
    type Msg = M;

    fn run(&self, ctx: ActorContext<M>) -> impl Future<Output = ActorResult> + Send {
        (self.f)(ctx)
    }
}
