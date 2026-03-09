use std::{future::Future, pin::Pin, sync::Arc};

use crate::{context::ChildContext, restart::Restart, shutdown::ShutdownPolicy};

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

pub type ChildResult = Result<(), BoxError>;

pub(crate) type ChildFuture = Pin<Box<dyn Future<Output = ChildResult> + Send + 'static>>;

#[derive(Clone)]
pub struct ChildSpec {
    pub(crate) inner: Arc<ChildSpecInner>,
}

#[derive(Clone)]
pub(crate) struct ChildSpecInner {
    pub(crate) id: Arc<str>,
    pub(crate) restart: Restart,
    pub(crate) shutdown_policy: ShutdownPolicy,
    pub(crate) factory: Arc<dyn ChildFactory>,
}

pub(crate) trait ChildFactory: Send + Sync + 'static {
    fn make(&self, ctx: ChildContext) -> ChildFuture;
}

struct ClosureFactory<F> {
    f: F,
}

impl<F, Fut> ChildFactory for ClosureFactory<F>
where
    F: Fn(ChildContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ChildResult> + Send + 'static,
{
    fn make(&self, ctx: ChildContext) -> ChildFuture {
        Box::pin((self.f)(ctx))
    }
}

fn make_child_factory<F, Fut>(f: F) -> Arc<dyn ChildFactory>
where
    F: Fn(ChildContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ChildResult> + Send + 'static,
{
    Arc::new(ClosureFactory { f })
}

impl ChildSpec {
    pub fn new<F, Fut>(id: impl Into<Arc<str>>, f: F) -> Self
    where
        F: Fn(ChildContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChildResult> + Send + 'static,
    {
        Self {
            inner: Arc::new(ChildSpecInner {
                id: id.into(),
                restart: Restart::default(),
                shutdown_policy: ShutdownPolicy::default(),
                factory: make_child_factory(f),
            }),
        }
    }

    pub fn restart(self, restart: Restart) -> Self {
        let mut inner = Arc::unwrap_or_clone(self.inner);
        inner.restart = restart;
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn shutdown(self, policy: ShutdownPolicy) -> Self {
        let mut inner = Arc::unwrap_or_clone(self.inner);
        inner.shutdown_policy = policy;
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn restart_policy(&self) -> Restart {
        self.inner.restart
    }

    pub fn shutdown_policy(&self) -> ShutdownPolicy {
        self.inner.shutdown_policy
    }
}
