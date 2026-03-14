use std::{collections::HashMap, sync::Arc};

use tokio::sync::{broadcast, watch};
use tokio_actor::{
    ActorRef, ActorRegistry, ActorSpec, IngressHandle, RunnableActor, RunnableActorFactory,
};
use tokio_supervisor::{
    ChildSpec, ControlError, Restart, RestartIntensity, ShutdownPolicy, Supervisor,
    SupervisorError, SupervisorEvent, SupervisorExit, SupervisorHandle, SupervisorSnapshot,
};

use crate::error::DynamicActorError;

#[derive(Clone, Debug)]
struct DynamicRuntimeState {
    registry: ActorRegistry,
    actor_factory: RunnableActorFactory,
}

impl DynamicRuntimeState {
    fn build_actor(
        &self,
        spec: ActorSpec,
        options: &DynamicActorOptions,
    ) -> Result<RunnableActor, DynamicActorError> {
        let actor = if options.peer_ids.is_empty() {
            self.actor_factory.actor(spec)
        } else {
            self.actor_factory
                .actor_with_peer_ids(spec, &self.registry, &options.peer_ids)?
        };
        actor.set_registry(self.registry.clone());
        Ok(actor)
    }
}

/// Options applied when adding a runtime actor to a supervised runtime.
#[derive(Clone, Debug)]
pub struct DynamicActorOptions {
    /// Restart policy for the supervised actor child.
    pub restart: Restart,
    /// Shutdown policy for the supervised actor child.
    pub shutdown: ShutdownPolicy,
    /// Optional restart intensity override for this actor child.
    pub restart_intensity: Option<RestartIntensity>,
    /// Initial peer ids that should be available through `ctx.peer()` /
    /// `ctx.send()` inside the dynamic actor.
    pub peer_ids: Vec<String>,
}

impl Default for DynamicActorOptions {
    fn default() -> Self {
        Self {
            restart: Restart::Transient,
            shutdown: ShutdownPolicy::default(),
            restart_intensity: None,
            peer_ids: Vec::new(),
        }
    }
}

/// Configured-but-not-yet-running runtime that owns a supervisor and its
/// stable ingress handles.
pub struct Runtime {
    supervisor: Supervisor,
    ingresses: HashMap<String, IngressHandle>,
    dynamic: Option<Arc<DynamicRuntimeState>>,
}

impl Runtime {
    /// Creates a runtime from a supervisor and its ingress handles.
    pub fn new(supervisor: Supervisor, ingresses: HashMap<String, IngressHandle>) -> Self {
        Self {
            supervisor,
            ingresses,
            dynamic: None,
        }
    }

    pub(crate) fn with_dynamic(
        supervisor: Supervisor,
        ingresses: HashMap<String, IngressHandle>,
        registry: ActorRegistry,
        actor_factory: RunnableActorFactory,
    ) -> Self {
        Self {
            supervisor,
            ingresses,
            dynamic: Some(Arc::new(DynamicRuntimeState {
                registry,
                actor_factory,
            })),
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
        RuntimeHandle::new(self.supervisor.spawn(), self.ingresses, self.dynamic)
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
    dynamic: Option<Arc<DynamicRuntimeState>>,
}

impl RuntimeHandle {
    fn new(
        supervisor: SupervisorHandle,
        ingresses: HashMap<String, IngressHandle>,
        dynamic: Option<Arc<DynamicRuntimeState>>,
    ) -> Self {
        Self {
            supervisor,
            ingresses: Arc::new(ingresses),
            dynamic,
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

    /// Returns a stable actor reference for a registered actor id.
    pub fn actor_ref(&self, actor_id: &str) -> Option<ActorRef> {
        self.dynamic
            .as_ref()
            .and_then(|dynamic| dynamic.registry.actor_ref(actor_id))
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

    /// Adds a runtime actor to a supervised actor runtime.
    pub async fn add_actor(
        &self,
        spec: ActorSpec,
        options: DynamicActorOptions,
    ) -> Result<ActorRef, DynamicActorError> {
        let dynamic = self
            .dynamic
            .as_ref()
            .ok_or(DynamicActorError::Unsupported)?;
        let actor = dynamic.build_actor(spec, &options)?;
        let actor_ref = actor.actor_ref();

        actor.register_with(&dynamic.registry)?;

        let child = actor_child_spec(
            actor,
            options.restart,
            options.shutdown,
            options.restart_intensity,
        );
        if let Err(err) = self.supervisor.add_child(child).await {
            let _ = dynamic.registry.deregister(actor_ref.id());
            return Err(err.into());
        }

        Ok(actor_ref)
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

    /// Removes a runtime-registered actor from the supervised runtime.
    pub async fn remove_actor(&self, actor_id: &str) -> Result<(), DynamicActorError> {
        let dynamic = self
            .dynamic
            .as_ref()
            .ok_or(DynamicActorError::Unsupported)?;
        self.supervisor.remove_child(actor_id.to_owned()).await?;
        dynamic.registry.deregister(actor_id)?;
        Ok(())
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

#[cfg(feature = "console")]
impl RuntimeHandle {
    /// Returns a [`ConsoleBuilder`](tokio_otp_console::ConsoleBuilder) pre-wired
    /// with this runtime's snapshot and event channels.
    pub fn console(&self) -> tokio_otp_console::ConsoleBuilder {
        tokio_otp_console::Console::builder()
            .snapshots(self.supervisor.subscribe_snapshots())
            .events(self.supervisor.event_sender())
    }
}

impl std::fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandle")
            .field("ingresses", &self.ingresses.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

pub(crate) fn actor_child_spec(
    actor: RunnableActor,
    restart: Restart,
    shutdown: ShutdownPolicy,
    restart_intensity: Option<RestartIntensity>,
) -> ChildSpec {
    let actor_id = actor.id().to_owned();
    let mut child = ChildSpec::new(actor_id, move |ctx| {
        let actor = actor.clone();
        async move {
            actor
                .run_until(ctx.token.cancelled())
                .await
                .map_err(Into::into)
        }
    })
    .restart(restart)
    .shutdown(shutdown);

    if let Some(intensity) = restart_intensity {
        child = child.restart_intensity(intensity);
    }

    child
}
