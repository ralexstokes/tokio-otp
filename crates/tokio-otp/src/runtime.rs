use std::sync::Arc;

use tokio::sync::{broadcast, watch};
use tokio_actor::{
    ActorRef, ActorRegistry, RawActor, RebindPolicy, RegistryError, RunnableActor,
    RunnableActorFactory,
};
use tokio_supervisor::{
    ChildSpec, ControlError, Restart, RestartIntensity, RestartMonitor, RestartMonitorError,
    ShutdownPolicy, Supervisor, SupervisorError, SupervisorEvent, SupervisorHandle,
    SupervisorSnapshot,
};

use crate::error::DynamicActorError;

#[derive(Clone, Debug)]
struct DynamicRuntimeState {
    registry: ActorRegistry,
    actor_factory: RunnableActorFactory,
}

impl DynamicRuntimeState {
    fn build_actor<A: RawActor>(&self, actor_id: impl Into<String>, actor: A) -> RunnableActor {
        let actor = self.actor_factory.actor(actor_id, actor);
        actor.set_registry(self.registry.clone());
        actor
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
}

impl Default for DynamicActorOptions {
    fn default() -> Self {
        Self {
            restart: Restart::Transient,
            shutdown: ShutdownPolicy::default(),
            restart_intensity: None,
        }
    }
}

/// Configured-but-not-yet-running runtime that owns a supervisor and its
/// optional dynamic actor registry.
///
/// Start the runtime with [`spawn`](Self::spawn), which returns the
/// [`RuntimeHandle`] control surface. To drive the runtime in the foreground
/// while keeping that control surface, call `spawn()` and then
/// [`RuntimeHandle::wait`]. Use [`into_supervisor`](Self::into_supervisor)
/// as the explicit escape hatch to the raw [`Supervisor`].
pub struct Runtime {
    supervisor: Supervisor,
    dynamic: Option<Arc<DynamicRuntimeState>>,
}

impl Runtime {
    /// Starts building a supervised actor runtime.
    ///
    /// Provide a graph to run every graph actor as its own supervised child, or
    /// call [`RuntimeBuilder::dynamic`](crate::RuntimeBuilder::dynamic) to
    /// start with no actors and add them at runtime.
    ///
    /// See [`RuntimeBuilder`](crate::RuntimeBuilder) for an example.
    pub fn builder() -> crate::RuntimeBuilder {
        crate::RuntimeBuilder::new()
    }

    /// Creates a runtime from a supervisor.
    pub fn new(supervisor: Supervisor) -> Self {
        Self {
            supervisor,
            dynamic: None,
        }
    }

    pub(crate) fn with_dynamic(
        supervisor: Supervisor,
        registry: ActorRegistry,
        actor_factory: RunnableActorFactory,
    ) -> Self {
        Self {
            supervisor,
            dynamic: Some(Arc::new(DynamicRuntimeState {
                registry,
                actor_factory,
            })),
        }
    }

    /// Returns the underlying [`Supervisor`], for example for nesting via
    /// [`Supervisor::into_child_spec`].
    ///
    /// On a runtime built with
    /// [`RuntimeBuilder::dynamic`](crate::RuntimeBuilder::dynamic), this
    /// discards the dynamic actor registry: [`RuntimeHandle::add_actor`] and
    /// [`RuntimeHandle::actor_ref`] support is lost. Keep the full runtime and
    /// use [`spawn`](Self::spawn) if you need dynamic actor support.
    ///
    pub fn into_supervisor(self) -> Supervisor {
        self.supervisor
    }

    /// Spawns the supervisor in the background and returns a combined handle.
    pub fn spawn(self) -> RuntimeHandle {
        RuntimeHandle::new(self.supervisor.spawn(), self.dynamic)
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

/// Cheaply cloneable runtime control surface.
///
/// By delegation to the underlying [`SupervisorHandle`], dropping the last
/// handle clone requests graceful shutdown. Other clones keep the runtime
/// alive, so fire-and-forget operation requires keeping a handle alive.
#[derive(Clone)]
pub struct RuntimeHandle {
    supervisor: SupervisorHandle,
    dynamic: Option<Arc<DynamicRuntimeState>>,
}

impl RuntimeHandle {
    fn new(supervisor: SupervisorHandle, dynamic: Option<Arc<DynamicRuntimeState>>) -> Self {
        Self {
            supervisor,
            dynamic,
        }
    }

    /// Returns a clone of the underlying supervisor handle.
    pub fn supervisor_handle(&self) -> SupervisorHandle {
        self.supervisor.clone()
    }

    /// Returns a stable actor reference for a registered actor id.
    ///
    /// Returns [`DynamicActorError::Unsupported`] if the runtime was built
    /// without a dynamic actor registry (see
    /// [`RuntimeBuilder::dynamic`](crate::RuntimeBuilder::dynamic)), and
    /// [`DynamicActorError::Lookup`] if the registry has no matching actor.
    pub fn actor_ref<M: Send + 'static>(
        &self,
        actor_id: &str,
    ) -> Result<ActorRef<M>, DynamicActorError> {
        let dynamic = self
            .dynamic
            .as_ref()
            .ok_or(DynamicActorError::Unsupported)?;
        Ok(dynamic.registry.actor_ref(actor_id)?)
    }

    /// Requests a graceful shutdown of the supervisor.
    pub fn shutdown(&self) {
        self.supervisor.shutdown();
    }

    /// Requests a graceful shutdown and waits for the supervisor to stop.
    pub async fn shutdown_and_wait(&self) -> Result<(), SupervisorError> {
        self.supervisor.shutdown_and_wait().await
    }

    /// Adds a new child to the supervisor at runtime.
    pub async fn add_child(&self, child: ChildSpec) -> Result<(), ControlError> {
        self.supervisor.add_child(child).await
    }

    /// Adds a runtime actor to a supervised actor runtime.
    pub async fn add_actor<A: RawActor>(
        &self,
        actor_id: impl Into<String>,
        actor: A,
        options: DynamicActorOptions,
    ) -> Result<ActorRef<A::Msg>, DynamicActorError> {
        let dynamic = self
            .dynamic
            .as_ref()
            .ok_or(DynamicActorError::Unsupported)?;
        let actor = dynamic.build_actor(actor_id, actor);
        let actor_ref = actor.actor_ref::<A::Msg>()?;

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

    /// Adds a child to a nested supervisor identified by `path`.
    pub async fn add_child_at<I, S>(&self, path: I, child: ChildSpec) -> Result<(), ControlError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.supervisor.add_child_at(path, child).await
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
        match self.supervisor.remove_child(actor_id.to_owned()).await {
            Ok(()) => {
                terminate_and_deregister_if_present(&dynamic.registry, actor_id)?;
                Ok(())
            }
            Err(ControlError::ShutdownTimedOut(id)) if id == actor_id => {
                let _ = dynamic.registry.terminate_and_deregister(actor_id);
                Err(ControlError::ShutdownTimedOut(id).into())
            }
            Err(err) => Err(err.into()),
        }
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

    /// Waits for the supervisor to stop.
    pub async fn wait(&self) -> Result<(), SupervisorError> {
        self.supervisor.wait().await
    }

    /// Delegates to [`SupervisorHandle::monitor_restart`].
    pub fn monitor_restart(
        &self,
        id: impl Into<String>,
    ) -> Result<RestartMonitor, RestartMonitorError> {
        self.supervisor.monitor_restart(id)
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
        f.debug_struct("RuntimeHandle").finish_non_exhaustive()
    }
}

/// Terminates the actor's binding when the supervisor drops the child spec —
/// the point after which no further restart can happen (restart intensity
/// exhausted, child removed, or supervisor exit). Without this, a Permanent
/// or Transient actor whose last run failed leaves its binding `Unbound` and
/// senders wait forever for a rebind.
struct TerminateBindingOnDrop {
    actor: RunnableActor,
}

impl Drop for TerminateBindingOnDrop {
    fn drop(&mut self) {
        self.actor.terminate_binding();
    }
}

pub(crate) fn actor_child_spec(
    actor: RunnableActor,
    restart: Restart,
    shutdown: ShutdownPolicy,
    restart_intensity: Option<RestartIntensity>,
) -> ChildSpec {
    let actor_id = actor.id().to_owned();
    actor.set_rebind_policy(rebind_policy_for_restart(restart));
    let guard = Arc::new(TerminateBindingOnDrop { actor });
    let mut child = ChildSpec::new(actor_id, move |ctx| {
        let actor = guard.actor.clone();
        async move {
            actor
                .run_until(ctx.shutdown_token().cancelled())
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

fn rebind_policy_for_restart(restart: Restart) -> RebindPolicy {
    match restart {
        Restart::Permanent => RebindPolicy::Always,
        Restart::Transient => RebindPolicy::OnFailure,
        Restart::Temporary => RebindPolicy::Never,
    }
}

fn terminate_and_deregister_if_present(
    registry: &ActorRegistry,
    actor_id: &str,
) -> Result<(), RegistryError> {
    match registry.terminate_and_deregister(actor_id) {
        Ok(()) => Ok(()),
        Err(RegistryError::UnknownActorId(unknown)) if unknown == actor_id => Ok(()),
        Err(error) => Err(error),
    }
}
