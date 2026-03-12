use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use tokio::{
    sync::mpsc,
    task::{Id as TaskId, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    actor::{ActorResult, ActorSpecInner},
    blocking::{BlockingRuntime, BlockingRuntimeEvent},
    context::{ActorContext, ActorRef},
    envelope::Envelope,
    error::GraphError,
    ingress::{IngressBinding, IngressHandle, MailboxRef},
    observability::{ActorExitStatus, GraphObservability, GraphRunStatus, GraphShutdownCause},
};

type MailboxSenders = HashMap<Arc<str>, MailboxRef>;
type MailboxReceivers = HashMap<Arc<str>, mpsc::Receiver<Envelope>>;

pub(crate) struct IngressDefinition {
    pub(crate) target_actor: Arc<str>,
    pub(crate) binding: Arc<IngressBinding>,
}

pub(crate) struct GraphInner {
    pub(crate) actors: Vec<Arc<ActorSpecInner>>,
    pub(crate) links: HashMap<Arc<str>, Vec<Arc<str>>>,
    pub(crate) mailbox_capacity: usize,
    pub(crate) ingresses: HashMap<Arc<str>, IngressDefinition>,
    pub(crate) running: AtomicBool,
    pub(crate) observability: GraphObservability,
}

impl GraphInner {
    fn clear_ingresses(&self) {
        for (name, ingress) in &self.ingresses {
            if ingress.binding.clear() {
                self.observability
                    .emit_ingress_cleared(name, &ingress.target_actor);
            }
        }
    }
}

/// Immutable actor graph specification.
///
/// Clones of the same `Graph` share stable ingress bindings so handles created
/// via [`ingress`](Self::ingress) keep working across reruns of the same
/// graph. Because those bindings are shared, only one instance of a given
/// graph spec may run at a time; concurrent reruns return
/// [`GraphError::AlreadyRunning`].
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
        self.inner.ingresses.get(name).map(|definition| {
            IngressHandle::new(
                Arc::from(name),
                Arc::clone(&definition.target_actor),
                definition.binding.subscribe(),
                self.inner.observability.clone(),
            )
        })
    }

    /// Runs the graph until the provided shutdown future resolves.
    ///
    /// Actor failures and panics fail the whole graph. A clean actor exit
    /// before shutdown is also treated as a graph failure because the crate
    /// does not provide internal supervision.
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
        if inner
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(GraphError::AlreadyRunning);
        }
        Ok(Self {
            inner: Arc::clone(inner),
        })
    }
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        self.inner.clear_ingresses();
        self.inner.running.store(false, Ordering::Release);
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
        self.bind_ingresses_with(&mailboxes)?;
        self.spawn_actors(&mailboxes, &mut receivers)?;

        let mut shutdown_requested = false;
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

                            if !shutdown_requested || exit.result.is_err() {
                                self.request_shutdown(exit.shutdown_cause(shutdown_requested));
                            }

                            if let Some(error) = classify_actor_exit(exit, shutdown_requested) {
                                failure.get_or_insert(error);
                            }
                        }
                        Err(error) => {
                            self.request_shutdown(graph_shutdown_cause_for_error(&error));
                            failure.get_or_insert(error);
                        }
                    }
                }
                _ = shutdown.as_mut(), if !shutdown_requested => {
                    shutdown_requested = true;
                    self.request_shutdown(GraphShutdownCause::External);
                }
            }
        }

        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn request_shutdown(&self, cause: GraphShutdownCause) {
        if self.shutdown.is_cancelled() {
            return;
        }

        self.inner.observability.emit_shutdown_requested(cause);
        self.shutdown.cancel();
        self.inner.clear_ingresses();
    }

    fn create_mailboxes(&self) -> (MailboxSenders, MailboxReceivers) {
        let mut mailboxes = HashMap::with_capacity(self.inner.actors.len());
        let mut receivers = HashMap::with_capacity(self.inner.actors.len());

        for actor in &self.inner.actors {
            let (sender, receiver) = mpsc::channel(self.inner.mailbox_capacity);
            let id = Arc::clone(&actor.id);
            mailboxes.insert(Arc::clone(&id), MailboxRef::new(id.clone(), sender));
            receivers.insert(id, receiver);
        }

        (mailboxes, receivers)
    }

    fn bind_ingresses_with(&self, mailboxes: &MailboxSenders) -> Result<(), GraphError> {
        for (name, ingress) in &self.inner.ingresses {
            let mailbox = mailboxes
                .get(&ingress.target_actor)
                .cloned()
                .ok_or_else(|| GraphError::InvalidState {
                    detail: format!(
                        "ingress `{name}` references missing actor mailbox `{}`",
                        ingress.target_actor
                    ),
                })?;
            if ingress.binding.bind(mailbox) {
                self.inner
                    .observability
                    .emit_ingress_bound(name, &ingress.target_actor);
            }
        }
        Ok(())
    }

    fn peer_refs(
        &self,
        actor_id: &Arc<str>,
        mailboxes: &MailboxSenders,
    ) -> Result<HashMap<Arc<str>, ActorRef>, GraphError> {
        let linked_peers =
            self.inner
                .links
                .get(actor_id)
                .ok_or_else(|| GraphError::InvalidState {
                    detail: format!("actor `{actor_id}` is missing its link definition"),
                })?;
        let mut peers = HashMap::with_capacity(linked_peers.len());

        for peer_id in linked_peers {
            let mailbox =
                mailboxes
                    .get(peer_id)
                    .cloned()
                    .ok_or_else(|| GraphError::InvalidState {
                        detail: format!(
                            "actor `{actor_id}` references missing peer mailbox `{peer_id}`"
                        ),
                    })?;
            peers.insert(
                Arc::clone(peer_id),
                ActorRef::from_mailbox(
                    mailbox,
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

    fn actor_ref(
        &self,
        actor_id: &Arc<str>,
        mailboxes: &MailboxSenders,
    ) -> Result<ActorRef, GraphError> {
        mailboxes
            .get(actor_id)
            .cloned()
            .map(|mailbox| {
                ActorRef::from_mailbox(
                    mailbox,
                    self.inner.observability.clone(),
                    Some(Arc::clone(actor_id)),
                )
            })
            .ok_or_else(|| GraphError::InvalidState {
                detail: format!("actor `{actor_id}` is missing its own mailbox sender"),
            })
    }

    fn spawn_actors(
        &mut self,
        mailboxes: &MailboxSenders,
        receivers: &mut MailboxReceivers,
    ) -> Result<(), GraphError> {
        for actor in &self.inner.actors {
            let peers = self.peer_refs(&actor.id, mailboxes)?;
            let mailbox = self.mailbox_receiver(&actor.id, receivers)?;
            let myself = self.actor_ref(&actor.id, mailboxes)?;
            let actor_shutdown = self.shutdown.child_token();
            let mut blocking = BlockingRuntime::new(
                actor.id.clone(),
                myself.clone(),
                actor_shutdown.clone(),
                self.inner.observability.clone(),
            );
            let ctx = ActorContext {
                id: actor.id.clone(),
                mailbox,
                peers,
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

            let abort_handle = self.join_set.spawn(async move {
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
            .instrument(actor_span));
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
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "<unknown>".to_owned());
                if err.is_panic() {
                    self.inner.observability.emit_actor_exited(
                        &actor_id,
                        ActorExitStatus::Panicked,
                        None,
                    );
                    Err(GraphError::ActorPanicked { actor_id })
                } else {
                    self.inner.observability.emit_actor_exited(
                        &actor_id,
                        ActorExitStatus::Cancelled,
                        None,
                    );
                    Err(GraphError::ActorCancelled { actor_id })
                }
            }
        }
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
