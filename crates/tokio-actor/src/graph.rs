use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    sync::mpsc,
    task::{Id as TaskId, JoinSet},
};
use tokio_util::sync::CancellationToken;

use crate::{
    actor::{ActorResult, ActorSpecInner},
    blocking::{BlockingRuntime, BlockingRuntimeEvent},
    context::{ActorContext, ActorRef},
    envelope::Envelope,
    error::GraphError,
    ingress::{IngressBinding, IngressHandle, MailboxRef},
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
}

impl GraphInner {
    fn clear_ingresses(&self) {
        for ingress in self.ingresses.values() {
            ingress.binding.clear();
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

    /// Returns a stable handle to a named ingress, if it exists.
    pub fn ingress(&self, name: &str) -> Option<IngressHandle> {
        self.inner.ingresses.get(name).map(|definition| {
            IngressHandle::new(
                Arc::from(name),
                Arc::clone(&definition.target_actor),
                definition.binding.subscribe(),
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

        let mut shutdown = std::pin::pin!(shutdown);
        runtime.run(&mut shutdown).await
    }
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph").finish_non_exhaustive()
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
                            if !shutdown_requested || exit.result.is_err() {
                                self.request_shutdown();
                            }

                            if let Some(error) = classify_actor_exit(exit, shutdown_requested) {
                                failure.get_or_insert(error);
                            }
                        }
                        Err(error) => {
                            self.request_shutdown();
                            failure.get_or_insert(error);
                        }
                    }
                }
                _ = shutdown.as_mut(), if !shutdown_requested => {
                    shutdown_requested = true;
                    self.request_shutdown();
                }
            }
        }

        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn request_shutdown(&self) {
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
            ingress.binding.bind(mailbox);
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
            peers.insert(Arc::clone(peer_id), ActorRef::from_mailbox(mailbox));
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
            .map(ActorRef::from_mailbox)
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
            let mut blocking =
                BlockingRuntime::new(actor.id.clone(), myself.clone(), actor_shutdown.clone());
            let ctx = ActorContext {
                id: actor.id.clone(),
                mailbox,
                peers,
                myself,
                shutdown: actor_shutdown.clone(),
                blocking: blocking.spawner(),
            };
            let actor_id = actor.id.clone();
            let factory = Arc::clone(&actor.factory);

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
            });
            self.task_ids.insert(abort_handle.id(), actor.id.clone());
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
                    Err(GraphError::ActorPanicked { actor_id })
                } else {
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

impl std::fmt::Debug for GraphInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphInner").finish_non_exhaustive()
    }
}
