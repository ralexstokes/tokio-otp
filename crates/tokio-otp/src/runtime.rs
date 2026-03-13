use std::{collections::HashMap, sync::Arc};

use tokio::sync::{broadcast, watch};
use tokio_actor::IngressHandle;
use tokio_supervisor::{
    ChildSpec, ControlError, Supervisor, SupervisorError, SupervisorEvent, SupervisorExit,
    SupervisorHandle, SupervisorSnapshot,
};

/// Configured-but-not-yet-running runtime that owns a supervisor and its
/// stable ingress handles.
pub struct Runtime {
    supervisor: Supervisor,
    ingresses: HashMap<String, IngressHandle>,
}

impl Runtime {
    /// Creates a runtime from a supervisor and its ingress handles.
    pub fn new(supervisor: Supervisor, ingresses: HashMap<String, IngressHandle>) -> Self {
        Self {
            supervisor,
            ingresses,
        }
    }

    /// Returns a clone of one ingress handle, if it exists.
    pub fn ingress(&self, name: &str) -> Option<IngressHandle> {
        self.ingresses.get(name).cloned()
    }

    /// Returns clones of every ingress handle.
    pub fn ingresses(&self) -> HashMap<String, IngressHandle> {
        self.ingresses.clone()
    }

    /// Returns the raw supervisor and ingress handles.
    pub fn into_parts(self) -> (Supervisor, HashMap<String, IngressHandle>) {
        (self.supervisor, self.ingresses)
    }

    /// Drives the runtime on the current task until the supervisor exits.
    pub async fn run(self) -> Result<SupervisorExit, SupervisorError> {
        self.supervisor.run().await
    }

    /// Spawns the supervisor in the background and returns a combined handle.
    pub fn spawn(self) -> RuntimeHandle {
        RuntimeHandle::new(self.supervisor.spawn(), self.ingresses)
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("ingresses", &self.ingresses.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Cheaply cloneable runtime control surface.
#[derive(Clone)]
pub struct RuntimeHandle {
    supervisor: SupervisorHandle,
    ingresses: Arc<HashMap<String, IngressHandle>>,
}

impl RuntimeHandle {
    fn new(supervisor: SupervisorHandle, ingresses: HashMap<String, IngressHandle>) -> Self {
        Self {
            supervisor,
            ingresses: Arc::new(ingresses),
        }
    }

    /// Returns a clone of one ingress handle, if it exists.
    pub fn ingress(&self, name: &str) -> Option<IngressHandle> {
        self.ingresses.get(name).cloned()
    }

    /// Returns clones of every ingress handle.
    pub fn ingresses(&self) -> HashMap<String, IngressHandle> {
        self.ingresses.as_ref().clone()
    }

    /// Returns a clone of the underlying supervisor handle.
    pub fn supervisor_handle(&self) -> SupervisorHandle {
        self.supervisor.clone()
    }

    /// Requests a graceful shutdown of the supervisor.
    pub fn shutdown(&self) {
        self.supervisor.shutdown();
    }

    /// Requests a graceful shutdown and waits for the supervisor to stop.
    pub async fn shutdown_and_wait(&self) -> Result<SupervisorExit, SupervisorError> {
        self.supervisor.shutdown_and_wait().await
    }

    /// Adds a new child to the supervisor at runtime.
    pub async fn add_child(&self, child: ChildSpec) -> Result<(), ControlError> {
        self.supervisor.add_child(child).await
    }

    /// Like [`Self::add_child`], but returns immediately if the control
    /// channel is full.
    pub async fn try_add_child(&self, child: ChildSpec) -> Result<(), ControlError> {
        self.supervisor.try_add_child(child).await
    }

    /// Adds a child to a nested supervisor identified by `path`.
    pub async fn add_child_at<I, S>(&self, path: I, child: ChildSpec) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.supervisor.add_child_at(path, child).await
    }

    /// Like [`Self::add_child_at`], but returns immediately if the target
    /// control channel is full.
    pub async fn try_add_child_at<I, S>(
        &self,
        path: I,
        child: ChildSpec,
    ) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.supervisor.try_add_child_at(path, child).await
    }

    /// Removes a child from the supervisor.
    pub async fn remove_child(&self, id: impl Into<String>) -> Result<(), ControlError> {
        self.supervisor.remove_child(id).await
    }

    /// Like [`Self::remove_child`], but returns immediately if the control
    /// channel is full.
    pub async fn try_remove_child(&self, id: impl Into<String>) -> Result<(), ControlError> {
        self.supervisor.try_remove_child(id).await
    }

    /// Removes a child from a nested supervisor identified by `path`.
    pub async fn remove_child_at<I, S>(
        &self,
        path: I,
        id: impl Into<String>,
    ) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.supervisor.remove_child_at(path, id).await
    }

    /// Like [`Self::remove_child_at`], but returns immediately if the target
    /// control channel is full.
    pub async fn try_remove_child_at<I, S>(
        &self,
        path: I,
        id: impl Into<String>,
    ) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.supervisor.try_remove_child_at(path, id).await
    }

    /// Waits for the supervisor to exit and returns its exit reason.
    pub async fn wait(&self) -> Result<SupervisorExit, SupervisorError> {
        self.supervisor.wait().await
    }

    /// Returns a new receiver for supervisor lifecycle events.
    pub fn subscribe(&self) -> broadcast::Receiver<SupervisorEvent> {
        self.supervisor.subscribe()
    }

    /// Returns a clone of the latest supervisor snapshot.
    pub fn snapshot(&self) -> SupervisorSnapshot {
        self.supervisor.snapshot()
    }

    /// Returns a watch receiver that updates when the snapshot changes.
    pub fn subscribe_snapshots(&self) -> watch::Receiver<SupervisorSnapshot> {
        self.supervisor.subscribe_snapshots()
    }
}

impl std::fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandle")
            .field("ingresses", &self.ingresses.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}
