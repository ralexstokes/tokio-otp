use std::{
    collections::HashSet,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    ActorFactory, ActorOptions, ActorRef, ActorStats, RawActor, RebindPolicy, RunnableActor,
    RunnableActorFactory,
};
use tokio::sync::{broadcast, watch};
use tokio_supervisor::{
    ChildSpec, ControlError, RestartIntensity, RestartMonitor, RestartMonitorError, RestartPolicy,
    RestartWatch, ShutdownPolicy, Supervisor, SupervisorError, SupervisorEvent, SupervisorHandle,
    SupervisorSnapshot, SupervisorSpec,
};

pub(crate) type ActorSubtrees = Vec<(String, Arc<ActorRuntimeState>)>;

#[derive(Clone, Debug)]
pub(crate) struct ActorRuntimeState {
    actor_factory: RunnableActorFactory,
    actors: Arc<Mutex<Vec<RunnableActor>>>,
    subtrees: Arc<Mutex<ActorSubtrees>>,
}

impl ActorRuntimeState {
    fn new(
        actor_factory: RunnableActorFactory,
        actors: Vec<RunnableActor>,
        subtrees: ActorSubtrees,
    ) -> Self {
        Self {
            actor_factory,
            actors: Arc::new(Mutex::new(actors)),
            subtrees: Arc::new(Mutex::new(subtrees)),
        }
    }

    fn actor_stats(&self, supervisor: &SupervisorHandle) -> Vec<ActorStats> {
        let snapshot = supervisor.snapshot();
        let child_ids = snapshot
            .children
            .iter()
            .map(|child| child.id.as_str())
            .collect::<HashSet<_>>();

        self.actors
            .lock()
            .expect("actor stats lock poisoned")
            .retain(|actor| child_ids.contains(actor.label()));
        self.subtrees
            .lock()
            .expect("actor subtree lock poisoned")
            .retain(|(id, _)| child_ids.contains(id.as_str()));

        let mut stats = self
            .actors
            .lock()
            .expect("actor stats lock poisoned")
            .iter()
            .map(RunnableActor::stats)
            .collect::<Vec<_>>();
        let subtrees = self
            .subtrees
            .lock()
            .expect("actor subtree lock poisoned")
            .iter()
            .map(|(id, subtree)| (id.clone(), Arc::clone(subtree)))
            .collect::<Vec<_>>();
        for (id, subtree) in subtrees {
            if let Some(supervisor) = supervisor.supervisor(&id) {
                stats.extend(subtree.actor_stats(&supervisor));
            }
        }
        stats
    }

    fn record_actor(&self, actor: RunnableActor) {
        let mut actors = self.actors.lock().expect("actor stats lock poisoned");
        actors.retain(|existing| existing.label() != actor.label());
        actors.push(actor);
    }

    fn forget_actor(&self, label: &str) {
        self.actors
            .lock()
            .expect("actor stats lock poisoned")
            .retain(|actor| actor.label() != label);
    }

    fn subtree(&self, id: &str) -> Option<Arc<ActorRuntimeState>> {
        self.subtrees
            .lock()
            .expect("actor subtree lock poisoned")
            .iter()
            .find(|(subtree_id, _)| subtree_id == id)
            .map(|(_, subtree)| Arc::clone(subtree))
    }

    fn forget_subtree(&self, id: &str) {
        self.subtrees
            .lock()
            .expect("actor subtree lock poisoned")
            .retain(|(subtree_id, _)| subtree_id != id);
    }

    fn forget_child(&self, id: &str) {
        self.forget_actor(id);
        self.forget_subtree(id);
    }
}

/// Options applied when adding a runtime actor to a supervised runtime.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DynamicActorOptions {
    /// Restart policy for the supervised actor child.
    pub restart: RestartPolicy,
    /// Shutdown policy for the supervised actor child.
    pub shutdown: ShutdownPolicy,
    /// Optional restart intensity override for this actor child.
    pub restart_intensity: Option<RestartIntensity>,
    // `None` selects the policy-dependent default. Keeping the override
    // unresolved makes `restart(...).remove_on_exit(...)` order-independent.
    remove_on_exit: Option<bool>,
}

impl Default for DynamicActorOptions {
    fn default() -> Self {
        Self {
            restart: RestartPolicy::OnFailure,
            shutdown: ShutdownPolicy::default(),
            restart_intensity: None,
            remove_on_exit: None,
        }
    }
}

impl DynamicActorOptions {
    /// Creates options with restart-on-failure and the default shutdown policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the actor's restart policy.
    #[must_use]
    pub fn restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    /// Sets the actor's shutdown policy.
    #[must_use]
    pub fn shutdown(mut self, shutdown: ShutdownPolicy) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Overrides the supervisor's restart intensity for this actor.
    #[must_use]
    pub fn restart_intensity(mut self, restart_intensity: RestartIntensity) -> Self {
        self.restart_intensity = Some(restart_intensity);
        self
    }

    /// Sets whether the actor child is removed after a terminal exit.
    ///
    /// Removal happens only when the restart policy declines a restart, never
    /// during a restart cycle. By default, actors using
    /// [`RestartPolicy::Never`] are removed and actors using other policies
    /// remain visible in supervisor snapshots.
    #[must_use]
    pub fn remove_on_exit(mut self, remove_on_exit: bool) -> Self {
        self.remove_on_exit = Some(remove_on_exit);
        self
    }

    fn resolved_remove_on_exit(&self) -> bool {
        self.remove_on_exit
            .unwrap_or(matches!(self.restart, RestartPolicy::Never))
    }
}

mod sealed {
    pub trait Sealed {}

    impl Sealed for tokio_supervisor::SupervisorHandle {}
}

/// Actor-aware extensions for any supervisor handle, including handles for
/// nested supervisors.
///
/// Import this trait to add a [`RunnableActor`] minted by
/// [`Graph::dynamic_factory`](crate::Graph::dynamic_factory) directly to a
/// running supervisor subtree.
///
/// This trait is sealed and cannot be implemented outside `tokio-otp`.
pub trait SupervisorHandleExt: sealed::Sealed {
    /// Adds a runnable actor as a supervised child.
    ///
    /// The actor's label becomes the child id. Its stable binding is rebound
    /// according to `options.restart`, and is terminated when the supervisor
    /// can no longer restart the child or the child is removed.
    ///
    /// Actors added through this extension are not tracked by
    /// [`RuntimeHandle::actor_stats`] or external observers built on it, even
    /// when this is the root handle returned by
    /// [`RuntimeHandle::supervisor_handle`]. Use [`RuntimeHandle::add_actor`]
    /// when runtime stats visibility matters.
    ///
    /// If adding fails, the actor's binding is left intact so the call can be
    /// retried with the same actor; senders on its ref keep waiting until an
    /// add succeeds.
    fn add_actor(
        &self,
        actor: RunnableActor,
        options: DynamicActorOptions,
    ) -> impl Future<Output = Result<(), ControlError>> + Send;
}

impl SupervisorHandleExt for SupervisorHandle {
    fn add_actor(
        &self,
        actor: RunnableActor,
        options: DynamicActorOptions,
    ) -> impl Future<Output = Result<(), ControlError>> + Send {
        let (child, termination) = actor_child_spec_with_termination(
            actor,
            options.restart,
            options.shutdown,
            options.restart_intensity,
            options.resolved_remove_on_exit(),
        );

        async move {
            let result = self.add_child(child).await;
            if result.is_err() {
                termination.disarm();
            }
            result
        }
    }
}

/// Configured-but-not-yet-running runtime that owns a supervisor and its
/// actor factory.
///
/// Start the runtime with [`spawn`](Self::spawn), which returns the
/// [`RuntimeHandle`] control surface. To drive the runtime in the foreground
/// while keeping that control surface, call `spawn()` and then
/// [`RuntimeHandle::wait`]. Use [`into_supervisor`](Self::into_supervisor)
/// as the explicit escape hatch to the raw [`Supervisor`].
pub struct Runtime {
    supervisor: Supervisor,
    actors: Arc<ActorRuntimeState>,
}

impl Runtime {
    /// Starts building a supervised actor runtime.
    ///
    /// Provide a graph to run every graph actor as its own supervised child,
    /// compose nested graphs with
    /// [`RuntimeBuilder::subtree`](crate::RuntimeBuilder::subtree), or build
    /// without one and add actors at runtime.
    ///
    /// See [`RuntimeBuilder`](crate::RuntimeBuilder) for an example.
    pub fn builder() -> crate::RuntimeBuilder {
        crate::RuntimeBuilder::new()
    }

    /// Creates a runtime from a supervisor.
    pub fn new(supervisor: Supervisor) -> Self {
        Self {
            supervisor,
            actors: Arc::new(ActorRuntimeState::new(
                RunnableActorFactory::new(),
                Vec::new(),
                Vec::new(),
            )),
        }
    }

    pub(crate) fn with_actor_tree(
        supervisor: Supervisor,
        actor_factory: RunnableActorFactory,
        actors: Vec<RunnableActor>,
        subtrees: ActorSubtrees,
    ) -> Self {
        Self {
            supervisor,
            actors: Arc::new(ActorRuntimeState::new(actor_factory, actors, subtrees)),
        }
    }

    pub(crate) fn into_parts(self) -> (Supervisor, Arc<ActorRuntimeState>) {
        (self.supervisor, self.actors)
    }

    /// Returns the underlying [`Supervisor`] for first-class nesting.
    ///
    /// This discards the actor factories and recursive actor metadata, so
    /// [`RuntimeHandle::add_actor`] and recursive actor stats are not available
    /// after converting to a raw supervisor. Keep the full runtime and use
    /// [`spawn`](Self::spawn) if you need actor-aware runtime behavior.
    ///
    pub fn into_supervisor(self) -> Supervisor {
        self.supervisor
    }

    /// Spawns the supervisor in the background and returns a combined handle.
    pub fn spawn(self) -> RuntimeHandle {
        RuntimeHandle::new(self.supervisor.spawn(), self.actors)
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
    actors: Arc<ActorRuntimeState>,
}

impl RuntimeHandle {
    fn new(supervisor: SupervisorHandle, actors: Arc<ActorRuntimeState>) -> Self {
        Self { supervisor, actors }
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
    pub async fn shutdown_and_wait(&self) -> Result<(), SupervisorError> {
        self.supervisor.shutdown_and_wait().await
    }

    /// Adds a new child to the supervisor at runtime.
    pub async fn add_child(&self, child: ChildSpec) -> Result<(), ControlError> {
        self.supervisor.add_child(child).await
    }

    /// Adds a nested supervisor at runtime.
    pub async fn add_supervisor(
        &self,
        id: impl Into<String>,
        supervisor: impl Into<SupervisorSpec>,
    ) -> Result<(), ControlError> {
        self.supervisor.add_supervisor(id, supervisor).await
    }

    /// Returns the stable handle for a direct nested supervisor.
    pub fn supervisor(&self, id: &str) -> Option<SupervisorHandle> {
        self.supervisor.supervisor(id)
    }

    /// Returns the actor-aware handle for a direct runtime subtree.
    ///
    /// Unlike [`supervisor`](Self::supervisor), this preserves the subtree's
    /// actor factory and recursive stats. It returns `None` for supervisors
    /// that were added through the raw supervisor APIs.
    pub fn subtree(&self, id: &str) -> Option<RuntimeHandle> {
        let supervisor = self.supervisor.supervisor(id)?;
        let actors = self.actors.subtree(id)?;
        Some(Self::new(supervisor, actors))
    }

    /// Adds a supervised runtime actor from an incarnation factory and returns
    /// its stable typed ref.
    ///
    /// The actor's label is also its direct supervisor child id, so it can be
    /// removed later with [`remove_child`](Self::remove_child). See
    /// [`ActorFactory`] for the incarnation lifecycle contract.
    pub async fn add_actor<F>(
        &self,
        label: impl Into<String>,
        factory: F,
        options: DynamicActorOptions,
    ) -> Result<ActorRef<<F::Actor as RawActor>::Msg>, ControlError>
    where
        F: ActorFactory,
    {
        let actor = self.actors.actor_factory.actor(label, factory);
        self.add_constructed_actor(actor, options).await
    }

    /// Adds a supervised runtime actor from an incarnation factory with explicit
    /// per-actor registration options and returns its stable typed ref.
    ///
    /// See [`ActorFactory`] for the incarnation lifecycle contract.
    pub async fn add_actor_with_options<F>(
        &self,
        label: impl Into<String>,
        factory: F,
        actor_options: ActorOptions<<F::Actor as RawActor>::Msg>,
        dynamic_options: DynamicActorOptions,
    ) -> Result<ActorRef<<F::Actor as RawActor>::Msg>, ControlError>
    where
        F: ActorFactory,
    {
        let actor = self
            .actors
            .actor_factory
            .actor_with_options(label, factory, actor_options);
        self.add_constructed_actor(actor, dynamic_options).await
    }

    async fn add_constructed_actor<M>(
        &self,
        (actor, actor_ref): (RunnableActor, ActorRef<M>),
        options: DynamicActorOptions,
    ) -> Result<ActorRef<M>, ControlError> {
        let child = actor_child_spec(
            actor.clone(),
            options.restart,
            options.shutdown,
            options.restart_intensity,
            options.resolved_remove_on_exit(),
        );
        self.supervisor.add_child(child).await?;
        self.actors.record_actor(actor);

        Ok(actor_ref)
    }

    /// Removes a child from the supervisor.
    ///
    /// Removal marks the membership as removing and starts its configured
    /// shutdown. When cooperative shutdown completes within its grace period,
    /// an [`Actor`](crate::Actor) stops its normal receive loop, applies its
    /// [`DrainPolicy`](crate::DrainPolicy), runs `on_stop`, makes the mailbox
    /// binding terminal, and is then detached. Immediate abort, or expiry of
    /// the cooperative grace period, can skip any remaining drain or hook work
    /// before detachment. The returned future completes after detachment (or
    /// after the configured shutdown backstop aborts it).
    ///
    /// A send racing with removal may still be accepted. With
    /// `DrainPolicy::Drain`, work accepted before drain closes intake belongs
    /// to the queued prefix handled before `on_stop`. With `Discard`, intake
    /// can remain open through `on_stop`, and accepted work can be dropped when
    /// the incarnation ends. Once drain closes intake, `try_send` may briefly
    /// return [`SendError::MailboxClosed`](crate::SendError::MailboxClosed),
    /// while an awaited `send` waits and then returns
    /// [`SendError::ActorTerminated`](crate::SendError::ActorTerminated).
    /// Removal does not return queued messages: end-to-end delivery ownership
    /// belongs in an application acknowledgement and replay protocol.
    pub async fn remove_child(&self, id: impl Into<String>) -> Result<(), ControlError> {
        let id = id.into();
        let result = self.supervisor.remove_child(id.clone()).await;
        if result.is_ok()
            || matches!(&result, Err(ControlError::ShutdownTimedOut(actor_id)) if actor_id == &id)
        {
            // Keep this eager cleanup even though `actor_stats` also
            // reconciles against snapshots. It gives the caller immediate
            // read-your-writes consistency, including timed-out removals
            // whose child may still be visible as `Removing` while winding
            // down.
            self.actors.forget_child(&id);
        }
        result
    }

    /// Waits for the supervisor to stop.
    pub async fn wait(&self) -> Result<(), SupervisorError> {
        self.supervisor.wait().await
    }

    /// Waits until all current actor children have completed `on_start`.
    pub async fn wait_started(&self) -> Result<(), SupervisorError> {
        self.supervisor.wait_started().await
    }

    /// Delegates to [`SupervisorHandle::monitor_restart`].
    pub fn monitor_restart(
        &self,
        id: impl Into<String>,
    ) -> Result<RestartMonitor, RestartMonitorError> {
        self.supervisor.monitor_restart(id)
    }

    /// Returns a new receiver for supervisor lifecycle events.
    ///
    /// Events are lossy observability, not durable control: see
    /// [`SupervisorHandle::subscribe`] for the full contract. Control logic
    /// such as restart breakers should use
    /// [`watch_restarts`](Self::watch_restarts) or
    /// [`subscribe_snapshots`](Self::subscribe_snapshots) instead.
    pub fn subscribe(&self) -> broadcast::Receiver<SupervisorEvent> {
        self.supervisor.subscribe()
    }

    /// Delegates to [`SupervisorHandle::watch_restarts`]: reliable,
    /// snapshot-backed observation of restart activity, suitable for control
    /// logic such as aggregate restart breakers.
    ///
    /// Covers the root supervisor's direct children only; watch a nested
    /// supervisor via [`supervisor`](Self::supervisor) to observe its
    /// subtree.
    pub fn watch_restarts(&self) -> RestartWatch {
        self.supervisor.watch_restarts()
    }

    /// Returns a clone of the latest supervisor snapshot.
    pub fn snapshot(&self) -> SupervisorSnapshot {
        self.supervisor.snapshot()
    }

    /// Returns point-in-time stats for this runtime and all nested runtime
    /// subtrees. This runtime's actors come first, followed recursively by each
    /// subtree in declaration order.
    ///
    /// Before sampling, tracked actors and subtrees are reconciled against the
    /// current supervisor snapshot. This removes stale entries after raw child
    /// removal or when a restarted subtree drops incarnation-local dynamic
    /// children.
    pub fn actor_stats(&self) -> Vec<ActorStats> {
        self.actors.actor_stats(&self.supervisor)
    }

    /// Returns a watch receiver that updates when the snapshot changes.
    pub fn subscribe_snapshots(&self) -> watch::Receiver<SupervisorSnapshot> {
        self.supervisor.subscribe_snapshots()
    }
}

impl std::fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandle").finish_non_exhaustive()
    }
}

/// Terminates the actor's binding when the supervisor drops the child spec —
/// the point after which no further restart can happen (restart intensity
/// exhausted, child removed, or supervisor exit). Without this, an `Always`
/// or `OnFailure` actor whose last run failed leaves its binding `Unbound` and
/// senders wait forever for a rebind.
struct TerminateBindingOnDrop {
    actor: RunnableActor,
    armed: AtomicBool,
}

impl TerminateBindingOnDrop {
    fn new(actor: RunnableActor) -> Self {
        Self {
            actor,
            armed: AtomicBool::new(true),
        }
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
    }
}

impl Drop for TerminateBindingOnDrop {
    fn drop(&mut self) {
        if self.armed.load(Ordering::Acquire) {
            self.actor.terminate_binding();
        }
    }
}

pub(crate) fn actor_child_spec(
    actor: RunnableActor,
    restart: RestartPolicy,
    shutdown: ShutdownPolicy,
    restart_intensity: Option<RestartIntensity>,
    remove_on_exit: bool,
) -> ChildSpec {
    actor_child_spec_with_termination(actor, restart, shutdown, restart_intensity, remove_on_exit).0
}

fn actor_child_spec_with_termination(
    actor: RunnableActor,
    restart: RestartPolicy,
    shutdown: ShutdownPolicy,
    restart_intensity: Option<RestartIntensity>,
    remove_on_exit: bool,
) -> (ChildSpec, Arc<TerminateBindingOnDrop>) {
    let actor_id = actor.label().to_owned();
    let rebind = rebind_policy_for_restart(restart);
    let guard = Arc::new(TerminateBindingOnDrop::new(actor));
    let child_guard = Arc::clone(&guard);
    let mut child = ChildSpec::new(actor_id, move |ctx| {
        let actor = child_guard.actor.clone();
        async move {
            actor
                .run_until_ready(ctx.shutdown_token().cancelled(), rebind, || {
                    ctx.mark_ready()
                })
                .await
                .map_err(Into::into)
        }
    })
    .wait_for_ready()
    .restart(restart)
    .remove_on_exit(remove_on_exit)
    .shutdown(shutdown);

    if let Some(intensity) = restart_intensity {
        child = child.restart_intensity(intensity);
    }

    (child, guard)
}

fn rebind_policy_for_restart(restart: RestartPolicy) -> RebindPolicy {
    match restart {
        RestartPolicy::Always => RebindPolicy::Always,
        RestartPolicy::OnFailure => RebindPolicy::OnFailure,
        RestartPolicy::Never => RebindPolicy::Never,
        // `RestartPolicy` is intentionally extensible. A future policy must
        // opt in to a more precise binding policy when this crate adopts it.
        _ => {
            tracing::warn!(
                "unknown restart policy; defaulting actor rebind behavior to on-failure"
            );
            RebindPolicy::OnFailure
        }
    }
}
