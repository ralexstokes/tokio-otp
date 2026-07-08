use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    future::Future,
    io::Error as IoError,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    actor::{Actor, BoxError},
    binding::{BindingCore, BindingLifecycle, RebindPolicy},
    builder::{
        DEFAULT_BLOCKING_SHUTDOWN_TIMEOUT, DEFAULT_MAILBOX_CAPACITY,
        DEFAULT_MAX_BLOCKING_TASKS_PER_ACTOR,
    },
    context::ActorRef,
    error::LookupError,
    graph::{ErasedRunner, GraphInner, RunnerStart, TypedRunner},
    observability::{ActorExitStatus, GraphObservability, anonymous_graph_name},
    registry::{ActorRegistry, RegistryError},
};

/// Errors returned from [`RunnableActor::run_until`].
#[derive(Debug, Error)]
pub enum ActorRunError {
    /// Another instance of the same runnable actor is already active.
    #[error("actor `{actor_id}` is already running")]
    AlreadyRunning { actor_id: String },
    /// The actor returned an error.
    #[error("actor `{actor_id}` returned an error")]
    Failed {
        actor_id: String,
        #[source]
        source: BoxError,
    },
}

/// Decomposed actor graph where each actor can be run independently.
///
/// Created via [`Graph::into_actor_set`](crate::Graph::into_actor_set). Once
/// decomposed, individual actors can be supervised as separate children (e.g.
/// by `tokio-supervisor`), each with its own restart and shutdown policies.
/// Stable typed refs remain functional across independent actor restarts.
#[derive(Clone)]
pub struct ActorSet {
    inner: Arc<ActorSetInner>,
}

struct ActorSetInner {
    graph: Arc<GraphInner>,
    actors: Vec<RunnableActor>,
    actor_index: HashMap<Arc<str>, usize>,
}

/// Factory for constructing runtime-added actors that share the same
/// per-graph execution settings as an existing actor set.
#[derive(Clone, Debug)]
pub struct RunnableActorFactory {
    observability: crate::observability::GraphObservability,
    mailbox_capacity: usize,
    max_blocking_tasks_per_actor: Option<usize>,
    blocking_shutdown_timeout: std::time::Duration,
}

impl ActorSet {
    pub(crate) fn from_graph(graph: Arc<GraphInner>) -> Self {
        let mut actors = Vec::with_capacity(graph.actors.len());
        let mut actor_index = HashMap::with_capacity(graph.actors.len());

        for (index, actor) in graph.actors.iter().enumerate() {
            actors.push(RunnableActor {
                inner: Arc::new(RunnableActorInner {
                    actor_id: actor.actor_id.clone(),
                    message_type: actor.message_type,
                    message_type_name: actor.message_type_name,
                    binding: actor.binding.clone(),
                    binding_lifecycle: actor.binding_lifecycle.clone(),
                    observability_slot: actor.observability.clone(),
                    runner: actor.runner.clone(),
                    registry: OnceLock::new(),
                    rebind_policy: std::sync::atomic::AtomicU8::new(RebindPolicy::Never as u8),
                    mailbox_capacity: graph.mailbox_capacity,
                    max_blocking_tasks_per_actor: graph.max_blocking_tasks_per_actor,
                    blocking_shutdown_timeout: graph.blocking_shutdown_timeout,
                    observability: graph.observability.clone(),
                    running: AtomicBool::new(false),
                }),
            });
            actor_index.insert(actor.actor_id.clone(), index);
        }

        Self {
            inner: Arc::new(ActorSetInner {
                graph,
                actors,
                actor_index,
            }),
        }
    }

    /// Returns an individually-runnable actor by id, if it exists.
    pub fn actor(&self, id: &str) -> Option<&RunnableActor> {
        self.inner
            .actor_index
            .get(id)
            .and_then(|index| self.inner.actors.get(*index))
    }

    /// Returns a typed actor ref for a decomposed graph actor.
    pub fn actor_ref<M: Send + 'static>(&self, id: &str) -> Result<ActorRef<M>, LookupError> {
        self.inner.graph.typed_actor_ref(id, None)
    }

    /// Returns all runnable actors in graph definition order.
    pub fn actors(&self) -> &[RunnableActor] {
        &self.inner.actors
    }

    /// Returns a factory for constructing additional runnable actors that use
    /// the same runtime configuration as this actor set.
    pub fn dynamic_factory(&self) -> RunnableActorFactory {
        RunnableActorFactory {
            observability: self.inner.graph.observability.clone(),
            mailbox_capacity: self.inner.graph.mailbox_capacity,
            max_blocking_tasks_per_actor: self.inner.graph.max_blocking_tasks_per_actor,
            blocking_shutdown_timeout: self.inner.graph.blocking_shutdown_timeout,
        }
    }
}

impl std::fmt::Debug for ActorSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorSet").finish_non_exhaustive()
    }
}

/// A single actor extracted from a graph, ready to be run independently.
///
/// Retains stable mailbox bindings from the original graph, so [`ActorRef`]
/// handles keep working across restarts. Use [`run_until`](Self::run_until) to
/// drive the actor until a shutdown future resolves.
#[derive(Clone)]
pub struct RunnableActor {
    inner: Arc<RunnableActorInner>,
}

struct RunnableActorInner {
    actor_id: Arc<str>,
    message_type: TypeId,
    message_type_name: &'static str,
    binding: Arc<dyn Any + Send + Sync>,
    binding_lifecycle: Arc<dyn BindingLifecycle>,
    observability_slot: Arc<OnceLock<crate::observability::GraphObservability>>,
    runner: Arc<dyn ErasedRunner>,
    registry: OnceLock<ActorRegistry>,
    rebind_policy: std::sync::atomic::AtomicU8,
    mailbox_capacity: usize,
    max_blocking_tasks_per_actor: Option<usize>,
    blocking_shutdown_timeout: std::time::Duration,
    observability: crate::observability::GraphObservability,
    running: AtomicBool,
}

impl RunnableActor {
    /// Returns the actor id.
    pub fn id(&self) -> &str {
        &self.inner.actor_id
    }

    /// Returns a stable typed actor reference for this actor.
    pub fn actor_ref<M: Send + 'static>(&self) -> Result<ActorRef<M>, LookupError> {
        if self.inner.message_type != TypeId::of::<M>() {
            return Err(LookupError::MessageTypeMismatch {
                actor_id: self.id().to_owned(),
                registered: self.inner.message_type_name,
                requested: type_name::<M>(),
            });
        }

        let Ok(binding) = self.inner.binding.clone().downcast::<BindingCore<M>>() else {
            unreachable!("message type id already verified")
        };

        Ok(ActorRef::from_parts(
            self.inner.actor_id.clone(),
            binding.subscribe(),
            self.inner.observability_slot.clone(),
            None,
        ))
    }

    /// Registers this actor in a runtime registry.
    pub fn register_with(&self, registry: &ActorRegistry) -> Result<(), RegistryError> {
        registry.register_erased(
            self.inner.actor_id.clone(),
            self.inner.message_type,
            self.inner.message_type_name,
            self.inner.observability_slot.clone(),
            self.inner.binding.clone(),
            self.inner.binding_lifecycle.clone(),
        )
    }

    /// Sets the runtime actor registry visible to this actor.
    pub fn set_registry(&self, registry: ActorRegistry) {
        let _ = self.inner.registry.set(registry);
    }

    /// Sets how this actor's binding behaves after a run exits.
    pub fn set_rebind_policy(&self, policy: RebindPolicy) {
        self.inner
            .rebind_policy
            .store(policy as u8, Ordering::Release);
    }

    /// Marks the actor's binding terminated and removes it from its registry.
    ///
    /// Call this when no further run will be started — for example when a
    /// supervisor driving [`run_until`](Self::run_until) in a restart loop
    /// gives up — so senders fail fast with `ActorTerminated` instead of
    /// waiting for a rebind that will never come.
    pub fn terminate_binding(&self) {
        self.apply_run_disposition(RunDisposition::Terminate);
    }

    /// Runs this actor with a fresh mailbox until shutdown resolves.
    pub async fn run_until<F>(&self, shutdown: F) -> Result<(), ActorRunError>
    where
        F: Future<Output = ()>,
    {
        let _active_run = ActiveActorRun::start(&self.inner)?;
        let actor_id = self.inner.actor_id.clone();
        let rebind_policy = self.rebind_policy();
        let actor_shutdown = CancellationToken::new();
        let mut shutdown = std::pin::pin!(shutdown);
        let actor_span = self.inner.observability.actor_span(&actor_id);
        let registry = self.inner.registry.get().cloned();
        let mut actor_task = AbortOnDrop::new(tokio::spawn(
            self.inner
                .runner
                .start(RunnerStart {
                    shutdown: actor_shutdown.clone(),
                    mailbox_capacity: self.inner.mailbox_capacity,
                    max_blocking_tasks_per_actor: self.inner.max_blocking_tasks_per_actor,
                    blocking_shutdown_timeout: self.inner.blocking_shutdown_timeout,
                    registry,
                    observability: self.inner.observability.clone(),
                    rebind_policy,
                })
                .instrument(actor_span),
        ));
        let _cancel_actor_on_drop = CancelOnDrop::new(actor_shutdown.clone());

        self.inner.observability.emit_actor_started(&actor_id);

        let actor_join = actor_task.join();
        tokio::pin!(actor_join);
        let mut shutdown_requested = false;
        let result = loop {
            tokio::select! {
                biased;
                joined = &mut actor_join => break joined,
                _ = shutdown.as_mut(), if !shutdown_requested => {
                    shutdown_requested = true;
                    actor_shutdown.cancel();
                }
            }
        };

        match result {
            Ok(Ok(())) => {
                let status = if shutdown_requested {
                    ActorExitStatus::Shutdown
                } else {
                    ActorExitStatus::Stopped
                };
                self.apply_run_disposition(run_disposition(
                    rebind_policy,
                    shutdown_requested,
                    status,
                ));
                self.inner
                    .observability
                    .emit_actor_exited(&actor_id, status, None);
                Ok(())
            }
            Ok(Err(source)) => {
                let error = ActorRunError::Failed {
                    actor_id: actor_id.to_string(),
                    source,
                };
                self.apply_run_disposition(run_disposition(
                    rebind_policy,
                    shutdown_requested,
                    ActorExitStatus::Failed,
                ));
                self.inner.observability.emit_actor_exited(
                    &actor_id,
                    ActorExitStatus::Failed,
                    Some(&error.to_string()),
                );
                Err(error)
            }
            Err(err) if err.is_panic() => {
                self.apply_run_disposition(run_disposition(
                    rebind_policy,
                    shutdown_requested,
                    ActorExitStatus::Panicked,
                ));
                self.inner.observability.emit_actor_exited(
                    &actor_id,
                    ActorExitStatus::Panicked,
                    None,
                );
                std::panic::resume_unwind(err.into_panic());
            }
            Err(_err) => {
                let source: BoxError = Box::new(IoError::other(format!(
                    "actor `{actor_id}` task was cancelled"
                )));
                let error = ActorRunError::Failed {
                    actor_id: actor_id.to_string(),
                    source,
                };
                self.apply_run_disposition(run_disposition(
                    rebind_policy,
                    shutdown_requested,
                    ActorExitStatus::Cancelled,
                ));
                self.inner.observability.emit_actor_exited(
                    &actor_id,
                    ActorExitStatus::Cancelled,
                    Some(&error.to_string()),
                );
                Err(error)
            }
        }
    }

    fn rebind_policy(&self) -> RebindPolicy {
        match self.inner.rebind_policy.load(Ordering::Acquire) {
            value if value == RebindPolicy::Always as u8 => RebindPolicy::Always,
            value if value == RebindPolicy::OnFailure as u8 => RebindPolicy::OnFailure,
            _ => RebindPolicy::Never,
        }
    }

    fn apply_run_disposition(&self, disposition: RunDisposition) {
        match disposition {
            RunDisposition::ExpectRebind => self.inner.binding_lifecycle.unbind(),
            RunDisposition::Terminate => {
                self.inner.binding_lifecycle.terminate();
                if let Some(registry) = self.inner.registry.get() {
                    let _ = registry.deregister(&self.inner.actor_id);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunDisposition {
    ExpectRebind,
    Terminate,
}

fn run_disposition(
    policy: RebindPolicy,
    shutdown_requested: bool,
    status: ActorExitStatus,
) -> RunDisposition {
    if shutdown_requested || status == ActorExitStatus::Shutdown {
        return RunDisposition::Terminate;
    }

    match (policy, status) {
        (RebindPolicy::Always, ActorExitStatus::Stopped) => RunDisposition::ExpectRebind,
        (RebindPolicy::Always | RebindPolicy::OnFailure, ActorExitStatus::Failed)
        | (RebindPolicy::Always | RebindPolicy::OnFailure, ActorExitStatus::Panicked)
        | (RebindPolicy::Always | RebindPolicy::OnFailure, ActorExitStatus::Cancelled) => {
            RunDisposition::ExpectRebind
        }
        (RebindPolicy::Never, _)
        | (RebindPolicy::OnFailure, ActorExitStatus::Stopped)
        | (_, ActorExitStatus::Shutdown) => RunDisposition::Terminate,
    }
}

impl std::fmt::Debug for RunnableActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnableActor")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

impl RunnableActorFactory {
    /// Creates a factory with the same defaults [`GraphBuilder`](crate::GraphBuilder)
    /// uses for a graph without an explicit name.
    pub fn new() -> Self {
        Self {
            observability: GraphObservability::new(anonymous_graph_name()),
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            max_blocking_tasks_per_actor: Some(DEFAULT_MAX_BLOCKING_TASKS_PER_ACTOR),
            blocking_shutdown_timeout: DEFAULT_BLOCKING_SHUTDOWN_TIMEOUT,
        }
    }

    /// Constructs a runtime actor.
    pub fn actor<A: Actor>(&self, actor_id: impl Into<String>, actor: A) -> RunnableActor {
        let actor_id: Arc<str> = actor_id.into().into();
        let binding = Arc::new(BindingCore::<A::Msg>::new(actor_id.clone()));
        binding.set_observability(self.observability.clone());
        let observability_slot = binding.observability_slot();
        RunnableActor {
            inner: Arc::new(RunnableActorInner {
                actor_id,
                message_type: TypeId::of::<A::Msg>(),
                message_type_name: type_name::<A::Msg>(),
                binding: binding.clone(),
                binding_lifecycle: binding.clone(),
                observability_slot,
                runner: Arc::new(TypedRunner { actor, binding }),
                registry: OnceLock::new(),
                rebind_policy: std::sync::atomic::AtomicU8::new(RebindPolicy::Never as u8),
                mailbox_capacity: self.mailbox_capacity,
                max_blocking_tasks_per_actor: self.max_blocking_tasks_per_actor,
                blocking_shutdown_timeout: self.blocking_shutdown_timeout,
                observability: self.observability.clone(),
                running: AtomicBool::new(false),
            }),
        }
    }
}

impl Default for RunnableActorFactory {
    fn default() -> Self {
        Self::new()
    }
}

struct ActiveActorRun {
    inner: Arc<RunnableActorInner>,
}

impl ActiveActorRun {
    fn start(inner: &Arc<RunnableActorInner>) -> Result<Self, ActorRunError> {
        if inner
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ActorRunError::AlreadyRunning {
                actor_id: inner.actor_id.to_string(),
            });
        }

        Ok(Self {
            inner: Arc::clone(inner),
        })
    }
}

impl Drop for ActiveActorRun {
    fn drop(&mut self) {
        self.inner.running.store(false, Ordering::Release);
    }
}

struct AbortOnDrop<T> {
    handle: Option<JoinHandle<T>>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn join(&mut self) -> Result<T, tokio::task::JoinError> {
        let result = self
            .handle
            .as_mut()
            .expect("join handle is present until joined")
            .await;
        self.handle = None;
        result
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

struct CancelOnDrop {
    token: CancellationToken,
}

impl CancelOnDrop {
    fn new(token: CancellationToken) -> Self {
        Self { token }
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.token.cancel();
    }
}
