use std::{future::Future, sync::Arc};

use crate::{
    context::ChildContext,
    restart::Restart,
    runtime::child_factory::{ChildFactory, make_child_factory},
    shutdown::ShutdownPolicy,
};

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug)]
pub enum ChildResult {
    Completed,
    Failed(BoxError),
}

#[derive(Clone)]
pub struct ChildSpec {
    pub(crate) inner: Arc<ChildSpecInner>,
}

pub(crate) struct ChildSpecInner {
    pub(crate) id: String,
    pub(crate) restart: Restart,
    pub(crate) shutdown_policy: ShutdownPolicy,
    pub(crate) factory: Arc<dyn ChildFactory>,
}

impl ChildSpec {
    pub fn new<F, Fut>(id: impl Into<String>, f: F) -> Self
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
        Self {
            inner: Arc::new(ChildSpecInner {
                id: self.inner.id.clone(),
                restart,
                shutdown_policy: self.inner.shutdown_policy.clone(),
                factory: Arc::clone(&self.inner.factory),
            }),
        }
    }

    pub fn shutdown(self, policy: ShutdownPolicy) -> Self {
        Self {
            inner: Arc::new(ChildSpecInner {
                id: self.inner.id.clone(),
                restart: self.inner.restart,
                shutdown_policy: policy,
                factory: Arc::clone(&self.inner.factory),
            }),
        }
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn restart_policy(&self) -> Restart {
        self.inner.restart
    }

    pub fn shutdown_policy(&self) -> &ShutdownPolicy {
        &self.inner.shutdown_policy
    }
}
