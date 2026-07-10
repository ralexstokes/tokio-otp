use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    future::Future,
    io::Error as IoError,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant as TokioInstant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    actor::{ActorResult, BoxError, RawActor},
    binding::{ActorStats, BindingCore, BindingGuard, BindingLifecycle, MailboxRef, RebindPolicy},
    builder::{DEFAULT_ACTOR_SHUTDOWN_TIMEOUT, DEFAULT_MAILBOX_CAPACITY},
    context::{ActorContext, ActorRef},
    error::LookupError,
    observability::{ActorExitStatus, GraphObservability, anonymous_graph_name},
    registry::{ActorRegistry, RegistryError},
};

pub(crate) type BoxedActorFuture = Pin<Box<dyn Future<Output = ActorResult> + Send + 'static>>;

pub(crate) struct RunnerStart {
    pub(crate) shutdown: CancellationToken,
    pub(crate) mailbox_capacity: usize,
    pub(crate) registry: Option<ActorRegistry>,
    pub(crate) observability: GraphObservability,
    pub(crate) rebind_policy: RebindPolicy,
}

/// Type-erased actor runner.
///
/// This is the only dyn layer in the crate: each implementation knows its own
/// message type and owns the typed binding core, so starting an actor binds a
/// typed mailbox without any downcast.
pub(crate) trait ErasedRunner: Send + Sync {
    fn start(&self, start: RunnerStart) -> BoxedActorFuture;
}

pub(crate) struct TypedRunner<A: RawActor> {
    pub(crate) actor: A,
    pub(crate) binding: Arc<BindingCore<A::Msg>>,
}

impl<A: RawActor> ErasedRunner for TypedRunner<A> {
    fn start(&self, start: RunnerStart) -> BoxedActorFuture {
        let actor_shutdown = start.shutdown;
        let observability = start.observability;
        let registry = start.registry;
        let (sender, mailbox) = mpsc::channel(start.mailbox_capacity);
        let actor_id = self.binding.actor_id().clone();
        let bound_mailbox = BindingGuard::bind(
            self.binding.clone(),
            MailboxRef::new(actor_id.clone(), sender),
            observability.clone(),
            start.rebind_policy,
        );
        let myself = ActorRef::from_core(&self.binding, Some(actor_id.clone()));
        let ctx = ActorContext {
            id: actor_id,
            mailbox,
            registry,
            myself,
            shutdown: actor_shutdown,
            observability,
        };
        let actor = self.actor.clone();

        Box::pin(async move {
            let _bound_mailbox = bound_mailbox;
            actor.run(ctx).await
        })
    }
}

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

/// An actor graph containing wiring and independently runnable actors.
///
/// Stable typed refs remain functional across independent actor restarts.
/// Execution is performed by driving the actors returned by [`actors`](Self::actors),
/// normally as separate supervisor children.
#[derive(Clone)]
pub struct Graph {
    inner: Arc<GraphInner>,
}

struct GraphInner {
    name: Arc<str>,
    actors: Vec<RunnableActor>,
    actor_index: HashMap<Arc<str>, usize>,
    observability: GraphObservability,
    mailbox_capacity: usize,
    actor_shutdown_timeout: Duration,
}

impl Graph {
    pub(crate) fn new(
        name: Arc<str>,
        actors: Vec<RunnableActor>,
        observability: GraphObservability,
        mailbox_capacity: usize,
        actor_shutdown_timeout: Duration,
    ) -> Self {
        let actor_index = actors
            .iter()
            .enumerate()
            .map(|(index, actor)| (actor.inner.actor_id.clone(), index))
            .collect();
        Self {
            inner: Arc::new(GraphInner {
                name,
                actors,
                actor_index,
                observability,
                mailbox_capacity,
                actor_shutdown_timeout,
            }),
        }
    }

    /// Returns the graph name used in tracing fields.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Returns an individually runnable actor by label, if it exists.
    pub fn actor(&self, label: &str) -> Option<&RunnableActor> {
        self.inner
            .actor_index
            .get(label)
            .and_then(|index| self.inner.actors.get(*index))
    }

    /// Returns a typed actor ref for a graph actor, checked at runtime.
    pub fn actor_ref<M: Send + 'static>(&self, label: &str) -> Result<ActorRef<M>, LookupError> {
        let actor = self.actor(label).ok_or_else(|| LookupError::UnknownActor {
            actor_id: label.to_owned(),
        })?;
        actor.actor_ref()
    }

    /// Returns all runnable actors in graph declaration order.
    pub fn actors(&self) -> &[RunnableActor] {
        &self.inner.actors
    }

    /// Returns point-in-time stats for every actor in graph declaration order.
    pub fn stats(&self) -> Vec<ActorStats> {
        self.inner.actors.iter().map(RunnableActor::stats).collect()
    }

    /// Returns a factory for constructing additional runnable actors that use
    /// the same runtime configuration as this graph.
    pub fn dynamic_factory(&self) -> RunnableActorFactory {
        RunnableActorFactory {
            observability: self.inner.observability.clone(),
            mailbox_capacity: self.inner.mailbox_capacity,
            actor_shutdown_timeout: self.inner.actor_shutdown_timeout,
        }
    }
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

/// A single actor in a graph, ready to be run independently.
///
/// Retains stable mailbox bindings from the graph, so [`ActorRef`] handles
/// keep working across restarts. Use [`run_until`](Self::run_until) to drive
/// one actor incarnation.
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
    runner: Arc<dyn ErasedRunner>,
    registry: OnceLock<ActorRegistry>,
    mailbox_capacity: usize,
    actor_shutdown_timeout: Duration,
    observability: GraphObservability,
    running: AtomicBool,
}

pub(crate) struct RunnableActorParts {
    pub(crate) actor_id: Arc<str>,
    pub(crate) message_type: TypeId,
    pub(crate) message_type_name: &'static str,
    pub(crate) binding: Arc<dyn Any + Send + Sync>,
    pub(crate) binding_lifecycle: Arc<dyn BindingLifecycle>,
    pub(crate) runner: Arc<dyn ErasedRunner>,
    pub(crate) mailbox_capacity: usize,
    pub(crate) actor_shutdown_timeout: Duration,
    pub(crate) observability: GraphObservability,
}

impl RunnableActor {
    pub(crate) fn new(parts: RunnableActorParts) -> Self {
        Self {
            inner: Arc::new(RunnableActorInner {
                actor_id: parts.actor_id,
                message_type: parts.message_type,
                message_type_name: parts.message_type_name,
                binding: parts.binding,
                binding_lifecycle: parts.binding_lifecycle,
                runner: parts.runner,
                registry: OnceLock::new(),
                mailbox_capacity: parts.mailbox_capacity,
                actor_shutdown_timeout: parts.actor_shutdown_timeout,
                observability: parts.observability,
                running: AtomicBool::new(false),
            }),
        }
    }

    /// Returns the actor label.
    pub fn label(&self) -> &str {
        &self.inner.actor_id
    }

    /// Returns a point-in-time snapshot of this actor's stats.
    pub fn stats(&self) -> ActorStats {
        self.inner.binding_lifecycle.stats()
    }

    /// Returns a stable typed actor reference for this actor.
    pub fn actor_ref<M: Send + 'static>(&self) -> Result<ActorRef<M>, LookupError> {
        if self.inner.message_type != TypeId::of::<M>() {
            return Err(LookupError::MessageTypeMismatch {
                actor_id: self.label().to_owned(),
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
            binding.stats_counters(),
            None,
        ))
    }

    /// Registers this actor in a runtime registry.
    pub fn register_with(&self, registry: &ActorRegistry) -> Result<(), RegistryError> {
        registry.register_erased(
            self.inner.actor_id.clone(),
            self.inner.message_type,
            self.inner.message_type_name,
            self.inner.binding.clone(),
            self.inner.binding_lifecycle.clone(),
        )
    }

    /// Sets the runtime actor registry visible to this actor.
    pub fn set_registry(&self, registry: ActorRegistry) {
        let _ = self.inner.registry.set(registry);
    }

    /// Marks the actor's binding terminated and removes it from its registry.
    ///
    /// Call this when no further run will be started so senders fail fast with
    /// `ActorTerminated` instead of waiting for a rebind that will never come.
    pub fn terminate_binding(&self) {
        self.apply_run_disposition(RunDisposition::Terminate);
    }

    /// Runs this actor with a fresh mailbox until shutdown resolves.
    ///
    /// `rebind` declares whether another incarnation is expected after this
    /// run. A hand-written host must call [`terminate_binding`](Self::terminate_binding)
    /// when it gives up after a policy that left the binding waiting to rebind.
    pub async fn run_until<F>(&self, shutdown: F, rebind: RebindPolicy) -> Result<(), ActorRunError>
    where
        F: Future<Output = ()>,
    {
        let _active_run = ActiveActorRun::start(&self.inner)?;
        let actor_id = self.inner.actor_id.clone();
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
                    registry,
                    observability: self.inner.observability.clone(),
                    rebind_policy: rebind,
                })
                .instrument(actor_span),
        ));
        let _cancel_actor_on_drop = CancelOnDrop::new(actor_shutdown.clone());

        self.inner.observability.emit_actor_started(&actor_id);

        let mut shutdown_requested = false;
        let mut shutdown_deadline = None;
        let mut aborted_after_timeout = false;
        let result = loop {
            tokio::select! {
                biased;
                joined = &mut actor_task => break joined,
                _ = shutdown.as_mut(), if !shutdown_requested => {
                    shutdown_requested = true;
                    actor_shutdown.cancel();
                    set_shutdown_deadline(
                        &mut shutdown_deadline,
                        self.inner.actor_shutdown_timeout,
                    );
                }
                _ = wait_for_shutdown_deadline(shutdown_deadline),
                    if shutdown_deadline.is_some() && !aborted_after_timeout =>
                {
                    aborted_after_timeout = true;
                    actor_task.abort();
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
                self.apply_run_disposition(run_disposition(rebind, shutdown_requested, status));
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
                    rebind,
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
                    rebind,
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
            Err(_err) if aborted_after_timeout => {
                self.apply_run_disposition(run_disposition(
                    rebind,
                    shutdown_requested,
                    ActorExitStatus::Cancelled,
                ));
                self.inner.observability.emit_actor_exited(
                    &actor_id,
                    ActorExitStatus::Cancelled,
                    None,
                );
                Ok(())
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
                    rebind,
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
            .field("label", &self.label())
            .finish_non_exhaustive()
    }
}

/// Factory for constructing runtime-added actors that share execution settings.
#[derive(Clone, Debug)]
pub struct RunnableActorFactory {
    observability: GraphObservability,
    mailbox_capacity: usize,
    actor_shutdown_timeout: Duration,
}

impl RunnableActorFactory {
    /// Creates a factory with the same defaults [`GraphBuilder`](crate::GraphBuilder)
    /// uses for a graph without an explicit name.
    pub fn new() -> Self {
        Self {
            observability: GraphObservability::new(anonymous_graph_name()),
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            actor_shutdown_timeout: DEFAULT_ACTOR_SHUTDOWN_TIMEOUT,
        }
    }

    /// Constructs a runtime actor.
    pub fn actor<A: RawActor>(&self, actor_id: impl Into<String>, actor: A) -> RunnableActor {
        let actor_id: Arc<str> = actor_id.into().into();
        let binding = Arc::new(BindingCore::<A::Msg>::new(actor_id.clone()));
        RunnableActor::new(RunnableActorParts {
            actor_id,
            message_type: TypeId::of::<A::Msg>(),
            message_type_name: type_name::<A::Msg>(),
            binding: binding.clone(),
            binding_lifecycle: binding.clone(),
            runner: Arc::new(TypedRunner { actor, binding }),
            mailbox_capacity: self.mailbox_capacity,
            actor_shutdown_timeout: self.actor_shutdown_timeout,
            observability: self.observability.clone(),
        })
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

    fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let handle = self
            .handle
            .as_mut()
            .expect("join handle is present until joined");
        match Pin::new(handle).poll(cx) {
            Poll::Ready(result) => {
                self.handle = None;
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.abort();
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

fn set_shutdown_deadline(deadline: &mut Option<TokioInstant>, timeout: Duration) {
    deadline.get_or_insert_with(|| TokioInstant::now() + timeout);
}

async fn wait_for_shutdown_deadline(deadline: Option<TokioInstant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}
