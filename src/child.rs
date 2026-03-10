use std::{future::Future, pin::Pin, sync::Arc};

use crate::{
    context::ChildContext,
    restart::{Restart, RestartIntensity},
    shutdown::ShutdownPolicy,
};

/// A type-erased, thread-safe error type used as the `Err` half of
/// [`ChildResult`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The result type returned by every supervised child function.
///
/// Returning `Ok(())` signals a clean exit. Returning an error signals a
/// failure, which may trigger a restart depending on the child's
/// [`Restart`] policy.
pub type ChildResult = Result<(), BoxError>;

pub(crate) type ChildFuture = Pin<Box<dyn Future<Output = ChildResult> + Send + 'static>>;

/// Specification for a supervised child task.
///
/// A `ChildSpec` pairs an async factory function with restart, shutdown, and
/// intensity policies. The factory is called each time the supervisor (re)starts
/// the child, receiving a fresh [`ChildContext`] with a new generation counter
/// and cancellation token.
///
/// The inner state is reference-counted, so cloning a `ChildSpec` is cheap and
/// shares the same factory.
#[derive(Clone)]
pub struct ChildSpec {
    pub(crate) inner: Arc<ChildSpecInner>,
}

#[derive(Clone)]
pub(crate) struct ChildSpecInner {
    pub(crate) id: String,
    pub(crate) restart: Restart,
    pub(crate) restart_intensity: Option<RestartIntensity>,
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
    fn map_inner(mut self, update: impl FnOnce(&mut ChildSpecInner)) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        update(inner);
        self
    }

    /// Creates a new child specification.
    ///
    /// `id` must be unique among siblings within the same supervisor.
    ///
    /// `f` is an async factory that is invoked each time the child is
    /// (re)started. It receives a [`ChildContext`] and should return
    /// `Ok(())` for a clean exit or an error for a failure.
    pub fn new<F, Fut>(id: impl Into<String>, f: F) -> Self
    where
        F: Fn(ChildContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChildResult> + Send + 'static,
    {
        Self {
            inner: Arc::new(ChildSpecInner {
                id: id.into(),
                restart: Restart::default(),
                restart_intensity: None,
                shutdown_policy: ShutdownPolicy::default(),
                factory: make_child_factory(f),
            }),
        }
    }

    /// Sets the restart policy for this child. See [`Restart`] for options.
    #[must_use]
    pub fn restart(self, restart: Restart) -> Self {
        self.map_inner(|inner| inner.restart = restart)
    }

    /// Sets the shutdown policy for this child. See [`ShutdownPolicy`] for
    /// options.
    #[must_use]
    pub fn shutdown(self, policy: ShutdownPolicy) -> Self {
        self.map_inner(|inner| inner.shutdown_policy = policy)
    }

    /// Overrides the supervisor-level [`RestartIntensity`] for this child.
    ///
    /// When set, this child tracks its own sliding restart window instead of
    /// sharing the supervisor's default.
    #[must_use]
    pub fn restart_intensity(self, intensity: RestartIntensity) -> Self {
        self.map_inner(|inner| inner.restart_intensity = Some(intensity))
    }

    /// Returns the child's unique identifier.
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    /// Returns the child's restart policy.
    pub fn restart_policy(&self) -> Restart {
        self.inner.restart
    }

    pub(crate) fn restart_intensity_override(&self) -> Option<RestartIntensity> {
        self.inner.restart_intensity
    }

    /// Returns the child's shutdown policy.
    pub fn shutdown_policy(&self) -> ShutdownPolicy {
        self.inner.shutdown_policy
    }
}
