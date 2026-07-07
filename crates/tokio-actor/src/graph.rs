use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    sync::mpsc,
    task::{Id as TaskId, JoinSet},
    time::{Instant as TokioInstant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    actor::{ActorResult, ActorSpecInner},
    actor_set::ActorSet,
    binding::{MailboxBinding, MailboxBindingGuard, MailboxRef},
    blocking::{BlockingRuntime, BlockingRuntimeEvent},
    context::{ActorContext, ActorRef},
    envelope::Envelope,
    error::GraphError,
    ingress::IngressHandle,
    observability::{ActorExitStatus, GraphObservability, GraphRunStatus, GraphShutdownCause},
};

type MailboxSenders = HashMap<Arc<str>, MailboxRef>;
type MailboxReceivers = HashMap<Arc<str>, mpsc::Receiver<Envelope>>;

const GRAPH_STATE_IDLE: u8 = 0;
const GRAPH_STATE_RUNNING: u8 = 1;
const GRAPH_STATE_DECOMPOSED: u8 = 2;

pub(crate) struct IngressDefinition {
    pub(crate) target_actor: Arc<str>,
}

pub(crate) struct GraphInner {
    pub(crate) actors: Vec<Arc<ActorSpecInner>>,
    pub(crate) links: HashMap<Arc<str>, Vec<Arc<str>>>,
    pub(crate) bindings: HashMap<Arc<str>, Arc<MailboxBinding>>,
    pub(crate) mailbox_capacity: usize,
    pub(crate) max_envelope_bytes: Option<usize>,
    pub(crate) max_blocking_tasks_per_actor: Option<usize>,
    pub(crate) actor_shutdown_timeout: Duration,
    pub(crate) blocking_shutdown_timeout: Duration,
    pub(crate) ingresses: HashMap<Arc<str>, IngressDefinition>,
    pub(crate) ingress_names_by_actor: HashMap<Arc<str>, Vec<Arc<str>>>,
    pub(crate) state: AtomicU8,
    pub(crate) observability: GraphObservability,
}

impl GraphInner {
    pub(crate) fn actor_binding(
        &self,
        actor_id: &Arc<str>,
    ) -> Result<Arc<MailboxBinding>, GraphError> {
        self.bindings
            .get(actor_id)
            .cloned()
            .ok_or_else(|| GraphError::InvalidState {
                detail: format!("actor `{actor_id}` is missing its mailbox binding"),
            })
    }

    pub(crate) fn ingress_names_for_actor(&self, actor_id: &Arc<str>) -> Vec<Arc<str>> {
        self.ingress_names_by_actor
            .get(actor_id)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn ingress_handle(&self, name: &str) -> Option<IngressHandle> {
        let (ingress_name, definition) = self.ingresses.get_key_value(name)?;
        let binding = self.bindings.get(&definition.target_actor)?;
        Some(IngressHandle::new(
            Arc::clone(ingress_name),
            Arc::clone(&definition.target_actor),
            binding.subscribe(),
            self.observability.clone(),
        ))
    }

    pub(crate) fn ingress_handles(&self) -> HashMap<String, IngressHandle> {
        self.ingresses
            .iter()
            .filter_map(|(name, definition)| {
                self.bindings.get(&definition.target_actor).map(|binding| {
                    (
                        name.to_string(),
                        IngressHandle::new(
                            Arc::clone(name),
                            Arc::clone(&definition.target_actor),
                            binding.subscribe(),
                            self.observability.clone(),
                        ),
                    )
                })
            })
            .collect()
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

/// Immutable actor graph specification.
///
/// Clones of the same `Graph` share stable mailbox bindings so actor and
/// ingress handles keep working across reruns of the same graph. Because those
/// bindings are shared, only one instance of a given graph spec may run at a
/// time; concurrent reruns return [`GraphError::AlreadyRunning`].
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
        self.inner.observability.graph_name()
    }

    /// Returns a stable handle to a named ingress, if it exists.
    pub fn ingress(&self, name: &str) -> Option<IngressHandle> {
        self.inner.ingress_handle(name)
    }

    /// Returns stable handles for every named ingress.
    pub fn ingresses(&self) -> HashMap<String, IngressHandle> {
        self.inner.ingress_handles()
    }

    /// Decomposes this graph into independently runnable actors.
    pub fn into_actor_set(self) -> Result<ActorSet, GraphError> {
        self.inner.mark_decomposed()?;
        ActorSet::from_graph(Arc::clone(&self.inner))
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
        let ingress_count = self.inner.ingresses.len();
        let mailbox_capacity = self.inner.mailbox_capacity;

        observability.emit_graph_started(actor_count, ingress_count, mailbox_capacity);

        let result = {
            let mut shutdown = std::pin::pin!(shutdown);
            runtime
                .run(&mut shutdown)
                .instrument(observability.graph_span(actor_count, ingress_count, mailbox_capacity))
                .await
        };

        let status = if result.is_ok() {
            GraphRunStatus::Ok
        } else {
            GraphRunStatus::Failed
        };
        let error = result.as_ref().err().map(std::string::ToString::to_string);
        observability.emit_graph_stopped(started_at.elapsed(), status, error.as_deref());

        result
    }
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
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
        let (mailboxes, mut receivers) = self.create_mailboxes();
        let mut bindings = self.bind_mailboxes(&mailboxes)?;
        self.spawn_actors(&mut receivers, &mut bindings)?;

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

    fn create_mailboxes(&self) -> (MailboxSenders, MailboxReceivers) {
        let mut mailboxes = HashMap::with_capacity(self.inner.actors.len());
        let mut receivers = HashMap::with_capacity(self.inner.actors.len());

        for actor in &self.inner.actors {
            let (sender, receiver) = mpsc::channel(self.inner.mailbox_capacity);
            let id = Arc::clone(&actor.id);
            mailboxes.insert(
                Arc::clone(&id),
                MailboxRef::new(id.clone(), sender, self.inner.max_envelope_bytes),
            );
            receivers.insert(id, receiver);
        }

        (mailboxes, receivers)
    }

    fn bind_mailboxes(
        &self,
        mailboxes: &MailboxSenders,
    ) -> Result<HashMap<Arc<str>, MailboxBindingGuard>, GraphError> {
        let mut bindings = HashMap::with_capacity(self.inner.actors.len());

        for actor in &self.inner.actors {
            let mailbox =
                mailboxes
                    .get(&actor.id)
                    .cloned()
                    .ok_or_else(|| GraphError::InvalidState {
                        detail: format!("actor `{}` is missing its mailbox sender", actor.id),
                    })?;
            let binding = self.inner.actor_binding(&actor.id)?;
            bindings.insert(
                Arc::clone(&actor.id),
                MailboxBindingGuard::bind(
                    Arc::clone(&actor.id),
                    binding,
                    mailbox,
                    self.inner.ingress_names_for_actor(&actor.id),
                    self.inner.observability.clone(),
                ),
            );
        }

        Ok(bindings)
    }

    fn peer_refs(&self, actor_id: &Arc<str>) -> Result<HashMap<Arc<str>, ActorRef>, GraphError> {
        let linked_peers =
            self.inner
                .links
                .get(actor_id)
                .ok_or_else(|| GraphError::InvalidState {
                    detail: format!("actor `{actor_id}` is missing its link definition"),
                })?;
        let mut peers = HashMap::with_capacity(linked_peers.len());

        for peer_id in linked_peers {
            let binding = self.inner.actor_binding(peer_id)?;
            peers.insert(
                Arc::clone(peer_id),
                ActorRef::from_binding(
                    Arc::clone(peer_id),
                    binding.subscribe(),
                    self.inner.observability.clone(),
                    Some(Arc::clone(actor_id)),
                ),
            );
        }

        Ok(peers)
    }

    fn mailbox_receiver(
        &self,
        actor_id: &Arc<str>,
        receivers: &mut MailboxReceivers,
    ) -> Result<mpsc::Receiver<Envelope>, GraphError> {
        receivers
            .remove(actor_id)
            .ok_or_else(|| GraphError::InvalidState {
                detail: format!("actor `{actor_id}` is missing its mailbox receiver"),
            })
    }

    fn actor_ref(&self, actor_id: &Arc<str>) -> Result<ActorRef, GraphError> {
        let binding = self.inner.actor_binding(actor_id)?;
        Ok(ActorRef::from_binding(
            Arc::clone(actor_id),
            binding.subscribe(),
            self.inner.observability.clone(),
            Some(Arc::clone(actor_id)),
        ))
    }

    fn spawn_actors(
        &mut self,
        receivers: &mut MailboxReceivers,
        bindings: &mut HashMap<Arc<str>, MailboxBindingGuard>,
    ) -> Result<(), GraphError> {
        for actor in &self.inner.actors {
            let peers = self.peer_refs(&actor.id)?;
            let mailbox = self.mailbox_receiver(&actor.id, receivers)?;
            let myself = self.actor_ref(&actor.id)?;
            let actor_shutdown = self.shutdown.child_token();
            let mut blocking = BlockingRuntime::new(
                actor.id.clone(),
                myself.clone(),
                actor_shutdown.clone(),
                self.inner.observability.clone(),
                self.inner.max_blocking_tasks_per_actor,
                self.inner.blocking_shutdown_timeout,
            );
            let ctx = ActorContext {
                id: actor.id.clone(),
                mailbox,
                peers,
                registry: None,
                myself,
                shutdown: actor_shutdown.clone(),
                blocking: blocking.spawner(),
                observability: self.inner.observability.clone(),
            };
            let actor_id = actor.id.clone();
            let factory = Arc::clone(&actor.factory);
            let actor_span = self
                .inner
                .observability
                .actor_span(&actor_id, ctx.peers.len());
            let bound_mailbox =
                bindings
                    .remove(&actor.id)
                    .ok_or_else(|| GraphError::InvalidState {
                        detail: format!("actor `{}` is missing its binding guard", actor.id),
                    })?;

            let abort_handle = self
                .join_set
                .spawn(
                    async move {
                        let _bound_mailbox = bound_mailbox;
                        let actor_future = factory.make(ctx);
                        tokio::pin!(actor_future);

                        let mut blocking_events_open = true;
                        let result = loop {
                            tokio::select! {
                                result = &mut actor_future => break result,
                                maybe_event = blocking.next_event(), if blocking_events_open => {
                                    if let Some(BlockingRuntimeEvent::Completed { task_id }) = maybe_event {
                                        if let Some(failure) = blocking.reap_task(task_id).await {
                                            actor_shutdown.cancel();
                                            break Err(Box::new(failure));
                                        }
                                    } else {
                                        blocking_events_open = false;
                                    }
                                }
                            }
                        };
                        let result = blocking.finish(result).await;
                        ActorTaskExit { actor_id, result }
                    }
                    .instrument(actor_span),
                );
            self.task_ids.insert(abort_handle.id(), actor.id.clone());
            self.inner.observability.emit_actor_started(&actor.id);
        }
        Ok(())
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

fn set_shutdown_deadline(deadline: &mut Option<TokioInstant>, timeout: Duration) {
    deadline.get_or_insert_with(|| TokioInstant::now() + timeout);
}

async fn wait_for_shutdown_deadline(deadline: Option<TokioInstant>) {
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
