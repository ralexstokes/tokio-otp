use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    sync::mpsc,
    task::{Id as TaskId, JoinHandle, JoinSet},
    time::{Instant as TokioInstant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    actor::{Actor, ActorResult},
    actor_set::ActorSet,
    binding::{BindingCore, BindingGuard, BindingLifecycle, MailboxRef, RebindPolicy},
    blocking::{BlockingRuntime, BlockingRuntimeEvent},
    context::{ActorContext, ActorRef},
    error::{GraphError, LookupError},
    observability::{ActorExitStatus, GraphObservability, GraphRunStatus, GraphShutdownCause},
    registry::ActorRegistry,
};

pub(crate) type BoxedActorFuture = Pin<Box<dyn Future<Output = ActorResult> + Send + 'static>>;

const GRAPH_STATE_IDLE: u8 = 0;
const GRAPH_STATE_RUNNING: u8 = 1;
const GRAPH_STATE_DECOMPOSED: u8 = 2;

pub(crate) struct GraphInner {
    pub(crate) name: Arc<str>,
    pub(crate) actors: Vec<GraphActor>,
    pub(crate) actor_index: HashMap<Arc<str>, usize>,
    pub(crate) mailbox_capacity: usize,
    pub(crate) max_blocking_tasks_per_actor: Option<usize>,
    pub(crate) actor_shutdown_timeout: Duration,
    pub(crate) blocking_shutdown_timeout: Duration,
    pub(crate) state: AtomicU8,
    pub(crate) observability: GraphObservability,
}

impl GraphInner {
    pub(crate) fn typed_actor_ref<M: Send + 'static>(
        &self,
        actor_id: &str,
        source_actor_id: Option<&Arc<str>>,
    ) -> Result<ActorRef<M>, LookupError> {
        let Some(&index) = self.actor_index.get(actor_id) else {
            return Err(LookupError::UnknownActor {
                actor_id: actor_id.to_owned(),
            });
        };
        let actor = &self.actors[index];
        if actor.message_type != TypeId::of::<M>() {
            return Err(LookupError::MessageTypeMismatch {
                actor_id: actor_id.to_owned(),
                registered: actor.message_type_name,
                requested: type_name::<M>(),
            });
        }
        let Ok(binding) = actor.binding.clone().downcast::<BindingCore<M>>() else {
            unreachable!("message type id already verified")
        };

        Ok(ActorRef::from_parts(
            actor.actor_id.clone(),
            binding.subscribe(),
            actor.observability.clone(),
            source_actor_id.cloned(),
        ))
    }

    fn mark_decomposed(&self) -> Result<(), GraphError> {
        match self.state.compare_exchange(
            GRAPH_STATE_IDLE,
            GRAPH_STATE_DECOMPOSED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(GRAPH_STATE_RUNNING) => Err(GraphError::AlreadyRunning),
            Err(GRAPH_STATE_DECOMPOSED) => Err(GraphError::InvalidState {
                detail: "graph has already been decomposed into an actor set".to_owned(),
            }),
            Err(_) => Err(GraphError::InvalidState {
                detail: "graph state transition failed".to_owned(),
            }),
        }
    }
}

pub(crate) struct GraphActor {
    pub(crate) actor_id: Arc<str>,
    pub(crate) message_type: TypeId,
    pub(crate) message_type_name: &'static str,
    pub(crate) binding: Arc<dyn Any + Send + Sync>,
    pub(crate) binding_lifecycle: Arc<dyn BindingLifecycle>,
    pub(crate) observability: Arc<OnceLock<GraphObservability>>,
    pub(crate) runner: Arc<dyn ErasedRunner>,
}

pub(crate) struct RunnerStart {
    pub(crate) shutdown: CancellationToken,
    pub(crate) mailbox_capacity: usize,
    pub(crate) max_blocking_tasks_per_actor: Option<usize>,
    pub(crate) blocking_shutdown_timeout: Duration,
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

pub(crate) struct TypedRunner<A: Actor> {
    pub(crate) actor: A,
    pub(crate) binding: Arc<BindingCore<A::Msg>>,
}

impl<A: Actor> ErasedRunner for TypedRunner<A> {
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
        let mut blocking = BlockingRuntime::new(
            actor_id.clone(),
            myself.clone(),
            actor_shutdown.clone(),
            observability.clone(),
            start.max_blocking_tasks_per_actor,
            start.blocking_shutdown_timeout,
        );
        let ctx = ActorContext {
            id: actor_id,
            mailbox,
            registry,
            myself,
            shutdown: actor_shutdown.clone(),
            blocking: blocking.spawner(),
            observability,
        };
        let actor = self.actor.clone();

        Box::pin(async move {
            let _bound_mailbox = bound_mailbox;
            let actor_future = actor.run(ctx);
            tokio::pin!(actor_future);

            let mut blocking_events_open = true;
            let result = loop {
                tokio::select! {
                    result = &mut actor_future => break result,
                    maybe_event = blocking.next_event(), if blocking_events_open => {
                        if let Some(BlockingRuntimeEvent::Completed { task_id }) = maybe_event {
                            if let Some(failure) = blocking.reap_task(task_id).await {
                                actor_shutdown.cancel();
                                break Err(Box::new(failure) as crate::actor::BoxError);
                            }
                        } else {
                            blocking_events_open = false;
                        }
                    }
                }
            };

            blocking.finish(result).await
        })
    }
}

/// Immutable actor graph specification.
///
/// Clones of the same `Graph` share stable mailbox bindings so actor refs keep
/// working across reruns of the same graph. Because those bindings are shared,
/// only one instance of a given graph spec may run at a time; concurrent
/// reruns return [`GraphError::AlreadyRunning`].
#[derive(Clone)]
pub struct Graph {
    inner: Arc<GraphInner>,
}

impl Graph {
    pub(crate) fn new(inner: GraphInner) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Returns the graph name used in tracing fields and metric labels.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Returns a typed actor ref for a graph actor, checked at runtime.
    pub fn actor_ref<M: Send + 'static>(&self, actor_id: &str) -> Result<ActorRef<M>, LookupError> {
        self.inner.typed_actor_ref(actor_id, None)
    }

    /// Decomposes this graph into independently runnable actors.
    pub fn into_actor_set(self) -> Result<ActorSet, GraphError> {
        self.inner.mark_decomposed()?;
        Ok(ActorSet::from_graph(Arc::clone(&self.inner)))
    }

    /// Runs the graph until the provided shutdown future resolves.
    ///
    /// Actor failures and panics fail the whole graph. A clean actor exit
    /// before shutdown is also treated as a graph failure because the crate
    /// does not provide internal supervision in graph-run mode.
    pub async fn run_until<F>(&self, shutdown: F) -> Result<(), GraphError>
    where
        F: Future<Output = ()>,
    {
        let _active_run = ActiveRun::start(&self.inner)?;
        let mut runtime = GraphRuntime::new(Arc::clone(&self.inner));
        let started_at = Instant::now();
        let observability = self.inner.observability.clone();
        let actor_count = self.inner.actors.len();
        let mailbox_capacity = self.inner.mailbox_capacity;

        observability.emit_graph_started(actor_count, mailbox_capacity);

        let result = {
            let mut shutdown = std::pin::pin!(shutdown);
            runtime
                .run(&mut shutdown)
                .instrument(observability.graph_span(actor_count, mailbox_capacity))
                .await
        };

        emit_graph_stopped(&observability, started_at, &result);

        result
    }

    /// Spawns the graph onto the current Tokio runtime and returns a handle.
    ///
    /// Actor refs may be used immediately; [`ActorRef::send`] waits until the
    /// target mailbox is bound.
    ///
    /// Panics if called outside a Tokio runtime, matching [`tokio::spawn`].
    pub fn spawn(&self) -> Result<GraphHandle, GraphError> {
        let active_run = ActiveRun::start(&self.inner)?;
        let mut runtime = GraphRuntime::new(Arc::clone(&self.inner));
        let started_at = Instant::now();
        let observability = self.inner.observability.clone();
        let actor_count = self.inner.actors.len();
        let mailbox_capacity = self.inner.mailbox_capacity;

        observability.emit_graph_started(actor_count, mailbox_capacity);

        let graph_span = observability.graph_span(actor_count, mailbox_capacity);
        {
            let _entered = graph_span.enter();
            runtime.spawn_actors();
        }

        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let graph_name = Arc::clone(&self.inner.name);
        let join = tokio::spawn(async move {
            let _active_run = active_run;
            let result = {
                let mut shutdown = std::pin::pin!(shutdown.cancelled());
                runtime.drive(&mut shutdown).instrument(graph_span).await
            };

            emit_graph_stopped(&observability, started_at, &result);
            result
        });

        Ok(GraphHandle {
            graph_name,
            stop,
            join,
        })
    }
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

/// Handle to a running actor graph, returned by [`Graph::spawn`].
///
/// The handle is not cloneable. [`wait`](Self::wait) and
/// [`shutdown_and_wait`](Self::shutdown_and_wait) consume it because graph
/// failures can contain non-cloneable error sources.
///
/// Dropping the handle does **not** shut down the graph. Call
/// [`shutdown`](Self::shutdown) explicitly, or use
/// [`shutdown_and_wait`](Self::shutdown_and_wait), to request graceful
/// shutdown.
pub struct GraphHandle {
    graph_name: Arc<str>,
    stop: CancellationToken,
    join: JoinHandle<Result<(), GraphError>>,
}

impl GraphHandle {
    /// Requests graceful shutdown of the graph.
    ///
    /// This is non-blocking and idempotent. Use [`wait`](Self::wait) or
    /// [`shutdown_and_wait`](Self::shutdown_and_wait) to await completion.
    pub fn shutdown(&self) {
        self.stop.cancel();
    }

    /// Waits for the graph to finish and returns its result.
    pub async fn wait(self) -> Result<(), GraphError> {
        match self.join.await {
            Ok(result) => result,
            Err(error) => Err(GraphError::InvalidState {
                detail: join_error_detail(error),
            }),
        }
    }

    /// Requests graceful shutdown and waits for the graph to finish.
    pub async fn shutdown_and_wait(self) -> Result<(), GraphError> {
        self.shutdown();
        self.wait().await
    }
}

impl std::fmt::Debug for GraphHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphHandle")
            .field("graph_name", &self.graph_name)
            .field("shutdown_requested", &self.stop.is_cancelled())
            .field("is_finished", &self.join.is_finished())
            .finish_non_exhaustive()
    }
}

fn join_error_detail(error: tokio::task::JoinError) -> String {
    if error.is_panic() {
        format!("graph drive task panicked: {error}")
    } else {
        format!("graph drive task was cancelled: {error}")
    }
}

fn emit_graph_stopped(
    observability: &GraphObservability,
    started_at: Instant,
    result: &Result<(), GraphError>,
) {
    let status = if result.is_ok() {
        GraphRunStatus::Ok
    } else {
        GraphRunStatus::Failed
    };
    let error = result.as_ref().err().map(std::string::ToString::to_string);
    observability.emit_graph_stopped(started_at.elapsed(), status, error.as_deref());
}

struct ActiveRun {
    inner: Arc<GraphInner>,
}

impl ActiveRun {
    fn start(inner: &Arc<GraphInner>) -> Result<Self, GraphError> {
        match inner.state.compare_exchange(
            GRAPH_STATE_IDLE,
            GRAPH_STATE_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(Self {
                inner: Arc::clone(inner),
            }),
            Err(GRAPH_STATE_RUNNING) => Err(GraphError::AlreadyRunning),
            Err(GRAPH_STATE_DECOMPOSED) => Err(GraphError::InvalidState {
                detail: "graph has already been decomposed into an actor set".to_owned(),
            }),
            Err(_) => Err(GraphError::InvalidState {
                detail: "graph state transition failed".to_owned(),
            }),
        }
    }
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        self.inner.state.store(GRAPH_STATE_IDLE, Ordering::Release);
    }
}

struct GraphRuntime {
    inner: Arc<GraphInner>,
    shutdown: CancellationToken,
    join_set: JoinSet<ActorTaskExit>,
    task_ids: HashMap<TaskId, Arc<str>>,
}

impl GraphRuntime {
    fn new(inner: Arc<GraphInner>) -> Self {
        Self {
            inner,
            shutdown: CancellationToken::new(),
            join_set: JoinSet::new(),
            task_ids: HashMap::new(),
        }
    }

    async fn run<F>(&mut self, shutdown: &mut std::pin::Pin<&mut F>) -> Result<(), GraphError>
    where
        F: Future<Output = ()>,
    {
        self.spawn_actors();
        self.drive(shutdown).await
    }

    async fn drive<F>(&mut self, shutdown: &mut std::pin::Pin<&mut F>) -> Result<(), GraphError>
    where
        F: Future<Output = ()>,
    {
        let mut shutdown_requested = false;
        let mut shutdown_deadline = None;
        let mut actors_aborted = false;
        let mut failure = None;

        while !self.join_set.is_empty() {
            tokio::select! {
                biased;
                joined = self.join_set.join_next_with_id() => {
                    let Some(joined) = joined else {
                        break;
                    };

                    let outcome = self.classify_join(joined);
                    match outcome {
                        Ok(exit) => {
                            let exit_status = exit.status(shutdown_requested);
                            let exit_error = exit.result.as_ref().err().map(std::string::ToString::to_string);
                            self.inner.observability.emit_actor_exited(
                                &exit.actor_id,
                                exit_status,
                                exit_error.as_deref(),
                            );

                            let should_request_shutdown =
                                !shutdown_requested || exit.result.is_err();
                            if should_request_shutdown
                                && self.request_shutdown(exit.shutdown_cause(shutdown_requested))
                            {
                                set_shutdown_deadline(
                                    &mut shutdown_deadline,
                                    self.inner.actor_shutdown_timeout,
                                );
                            }

                            if let Some(error) = classify_actor_exit(exit, shutdown_requested) {
                                failure.get_or_insert(error);
                            }
                        }
                        Err(error) => {
                            if actors_aborted && matches!(error, GraphError::ActorCancelled { .. }) {
                                continue;
                            }
                            if self.request_shutdown(graph_shutdown_cause_for_error(&error)) {
                                set_shutdown_deadline(
                                    &mut shutdown_deadline,
                                    self.inner.actor_shutdown_timeout,
                                );
                            }
                            failure.get_or_insert(error);
                        }
                    }
                }
                _ = shutdown.as_mut(), if !shutdown_requested => {
                    shutdown_requested = true;
                    if self.request_shutdown(GraphShutdownCause::External) {
                        set_shutdown_deadline(
                            &mut shutdown_deadline,
                            self.inner.actor_shutdown_timeout,
                        );
                    }
                }
                _ = wait_for_shutdown_deadline(shutdown_deadline), if shutdown_deadline.is_some() && !actors_aborted => {
                    actors_aborted = true;
                    self.join_set.abort_all();
                }
            }
        }

        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn request_shutdown(&self, cause: GraphShutdownCause) -> bool {
        if self.shutdown.is_cancelled() {
            return false;
        }

        self.inner.observability.emit_shutdown_requested(cause);
        self.shutdown.cancel();
        true
    }

    fn spawn_actors(&mut self) {
        for actor in &self.inner.actors {
            let actor_id = actor.actor_id.clone();
            let actor_shutdown = self.shutdown.child_token();
            let actor_future = actor.runner.start(RunnerStart {
                shutdown: actor_shutdown,
                mailbox_capacity: self.inner.mailbox_capacity,
                max_blocking_tasks_per_actor: self.inner.max_blocking_tasks_per_actor,
                blocking_shutdown_timeout: self.inner.blocking_shutdown_timeout,
                registry: None,
                observability: self.inner.observability.clone(),
                rebind_policy: RebindPolicy::Never,
            });
            let actor_span = self.inner.observability.actor_span(&actor_id);

            let abort_handle = self.join_set.spawn(
                async move {
                    let result = actor_future.await;
                    ActorTaskExit { actor_id, result }
                }
                .instrument(actor_span),
            );
            self.task_ids
                .insert(abort_handle.id(), actor.actor_id.clone());
            self.inner.observability.emit_actor_started(&actor.actor_id);
        }
    }

    fn classify_join(
        &mut self,
        joined: Result<(TaskId, ActorTaskExit), tokio::task::JoinError>,
    ) -> Result<ActorTaskExit, GraphError> {
        match joined {
            Ok((task_id, exit)) => {
                self.task_ids.remove(&task_id);
                Ok(exit)
            }
            Err(err) => {
                let actor_id = self
                    .task_ids
                    .remove(&err.id())
                    .unwrap_or_else(|| Arc::from("<unknown>"));
                if err.is_panic() {
                    self.inner.observability.emit_actor_exited(
                        &actor_id,
                        ActorExitStatus::Panicked,
                        None,
                    );
                    Err(GraphError::ActorPanicked {
                        actor_id: actor_id.to_string(),
                    })
                } else {
                    self.inner.observability.emit_actor_exited(
                        &actor_id,
                        ActorExitStatus::Cancelled,
                        None,
                    );
                    Err(GraphError::ActorCancelled {
                        actor_id: actor_id.to_string(),
                    })
                }
            }
        }
    }
}

pub(crate) fn set_shutdown_deadline(deadline: &mut Option<TokioInstant>, timeout: Duration) {
    deadline.get_or_insert_with(|| TokioInstant::now() + timeout);
}

pub(crate) async fn wait_for_shutdown_deadline(deadline: Option<TokioInstant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

struct ActorTaskExit {
    actor_id: Arc<str>,
    result: ActorResult,
}

impl ActorTaskExit {
    fn status(&self, shutdown_requested: bool) -> ActorExitStatus {
        match &self.result {
            Ok(()) if shutdown_requested => ActorExitStatus::Shutdown,
            Ok(()) => ActorExitStatus::Stopped,
            Err(_) => ActorExitStatus::Failed,
        }
    }

    fn shutdown_cause(&self, shutdown_requested: bool) -> GraphShutdownCause {
        match self.status(shutdown_requested) {
            ActorExitStatus::Shutdown => GraphShutdownCause::External,
            ActorExitStatus::Stopped => GraphShutdownCause::ActorStopped,
            ActorExitStatus::Failed => GraphShutdownCause::ActorFailed,
            ActorExitStatus::Panicked => GraphShutdownCause::ActorPanicked,
            ActorExitStatus::Cancelled => GraphShutdownCause::ActorCancelled,
        }
    }
}

fn classify_actor_exit(exit: ActorTaskExit, shutdown_requested: bool) -> Option<GraphError> {
    match exit.result {
        Ok(()) if shutdown_requested => None,
        Ok(()) => Some(GraphError::ActorStopped {
            actor_id: exit.actor_id.to_string(),
        }),
        Err(source) => Some(GraphError::ActorFailed {
            actor_id: exit.actor_id.to_string(),
            source,
        }),
    }
}

fn graph_shutdown_cause_for_error(error: &GraphError) -> GraphShutdownCause {
    match error {
        GraphError::ActorStopped { .. } => GraphShutdownCause::ActorStopped,
        GraphError::ActorFailed { .. } => GraphShutdownCause::ActorFailed,
        GraphError::ActorPanicked { .. } => GraphShutdownCause::ActorPanicked,
        GraphError::ActorCancelled { .. } => GraphShutdownCause::ActorCancelled,
        GraphError::AlreadyRunning | GraphError::InvalidState { .. } => {
            GraphShutdownCause::External
        }
    }
}

impl std::fmt::Debug for GraphInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphInner").finish_non_exhaustive()
    }
}
