use std::{
    collections::HashMap,
    future::Future,
    io::Error as IoError,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    actor::{ActorSpec, ActorSpecInner, BoxError},
    binding::{MailboxBinding, MailboxBindingGuard, MailboxRef},
    blocking::{BlockingRuntime, BlockingRuntimeEvent},
    context::{ActorContext, ActorRef},
    error::GraphError,
    graph::GraphInner,
    ingress::IngressHandle,
    observability::ActorExitStatus,
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
/// Stable peer wiring and ingress handles remain functional across independent
/// actor restarts.
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
    max_envelope_bytes: Option<usize>,
    max_blocking_tasks_per_actor: Option<usize>,
    blocking_shutdown_timeout: std::time::Duration,
}

impl ActorSet {
    pub(crate) fn from_graph(graph: Arc<GraphInner>) -> Result<Self, GraphError> {
        let mut actors = Vec::with_capacity(graph.actors.len());
        let mut actor_index = HashMap::with_capacity(graph.actors.len());

        for (index, actor) in graph.actors.iter().enumerate() {
            let peers = peer_refs(&graph, &actor.id)?;
            let binding = graph.actor_binding(&actor.id)?;
            let myself = ActorRef::from_binding(
                Arc::clone(&actor.id),
                binding.subscribe(),
                graph.observability.clone(),
                Some(Arc::clone(&actor.id)),
            );

            actors.push(RunnableActor {
                inner: Arc::new(RunnableActorInner {
                    spec: Arc::clone(actor),
                    binding,
                    peers,
                    myself,
                    registry: OnceLock::new(),
                    mailbox_capacity: graph.mailbox_capacity,
                    max_envelope_bytes: graph.max_envelope_bytes,
                    max_blocking_tasks_per_actor: graph.max_blocking_tasks_per_actor,
                    blocking_shutdown_timeout: graph.blocking_shutdown_timeout,
                    observability: graph.observability.clone(),
                    ingress_names: graph.ingress_names_for_actor(&actor.id),
                    running: AtomicBool::new(false),
                }),
            });
            actor_index.insert(Arc::clone(&actor.id), index);
        }

        Ok(Self {
            inner: Arc::new(ActorSetInner {
                graph,
                actors,
                actor_index,
            }),
        })
    }

    /// Returns an individually-runnable actor by id, if it exists.
    pub fn actor(&self, id: &str) -> Option<&RunnableActor> {
        self.inner
            .actor_index
            .get(id)
            .and_then(|index| self.inner.actors.get(*index))
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
            max_envelope_bytes: self.inner.graph.max_envelope_bytes,
            max_blocking_tasks_per_actor: self.inner.graph.max_blocking_tasks_per_actor,
            blocking_shutdown_timeout: self.inner.graph.blocking_shutdown_timeout,
        }
    }

    /// Returns a stable handle to a named ingress, if it exists.
    pub fn ingress(&self, name: &str) -> Option<IngressHandle> {
        self.inner.graph.ingress_handle(name)
    }

    /// Returns stable handles for every named ingress.
    pub fn ingresses(&self) -> HashMap<String, IngressHandle> {
        self.inner.graph.ingress_handles()
    }
}

impl std::fmt::Debug for ActorSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorSet").finish_non_exhaustive()
    }
}

/// A single actor extracted from a graph, ready to be run independently.
///
/// Retains stable peer wiring and mailbox bindings from the original graph,
/// so [`ActorRef`] and [`IngressHandle`] handles keep working across restarts.
/// Use [`run_until`](Self::run_until) to drive the actor until a shutdown
/// future resolves.
#[derive(Clone)]
pub struct RunnableActor {
    inner: Arc<RunnableActorInner>,
}

struct RunnableActorInner {
    spec: Arc<ActorSpecInner>,
    binding: Arc<MailboxBinding>,
    peers: HashMap<Arc<str>, ActorRef>,
    myself: ActorRef,
    registry: OnceLock<ActorRegistry>,
    mailbox_capacity: usize,
    max_envelope_bytes: Option<usize>,
    max_blocking_tasks_per_actor: Option<usize>,
    blocking_shutdown_timeout: std::time::Duration,
    observability: crate::observability::GraphObservability,
    ingress_names: Vec<Arc<str>>,
    running: AtomicBool,
}

impl RunnableActor {
    /// Returns the actor id.
    pub fn id(&self) -> &str {
        &self.inner.spec.id
    }

    /// Returns a stable actor reference for this actor.
    pub fn actor_ref(&self) -> ActorRef {
        ActorRef::from_binding(
            Arc::clone(&self.inner.spec.id),
            self.inner.binding.subscribe(),
            self.inner.observability.clone(),
            None,
        )
    }

    /// Registers this actor in a runtime registry.
    pub fn register_with(&self, registry: &ActorRegistry) -> Result<(), RegistryError> {
        registry.register_entry(
            Arc::clone(&self.inner.spec.id),
            Arc::clone(&self.inner.binding),
            self.inner.observability.clone(),
        )
    }

    /// Sets the runtime actor registry visible to this actor.
    pub fn set_registry(&self, registry: ActorRegistry) {
        let _ = self.inner.registry.set(registry);
    }

    /// Runs this actor with a fresh mailbox until shutdown resolves.
    pub async fn run_until<F>(&self, shutdown: F) -> Result<(), ActorRunError>
    where
        F: Future<Output = ()>,
    {
        let _active_run = ActiveActorRun::start(&self.inner)?;
        let actor_id = Arc::clone(&self.inner.spec.id);
        let (sender, receiver) = mpsc::channel(self.inner.mailbox_capacity);
        let mailbox = MailboxRef::new(Arc::clone(&actor_id), sender, self.inner.max_envelope_bytes);
        let bound_mailbox = MailboxBindingGuard::bind(
            Arc::clone(&actor_id),
            Arc::clone(&self.inner.binding),
            mailbox,
            self.inner.ingress_names.clone(),
            self.inner.observability.clone(),
        );
        let actor_shutdown = CancellationToken::new();
        let mut shutdown = std::pin::pin!(shutdown);
        let actor_span = self
            .inner
            .observability
            .actor_span(&actor_id, self.inner.peers.len());
        let registry = self.inner.registry.get().cloned();
        let mut actor_task = AbortOnDrop::new(tokio::spawn({
            let actor_id = Arc::clone(&actor_id);
            let spec = Arc::clone(&self.inner.spec);
            let peers = self.inner.peers.clone();
            let myself = self.inner.myself.clone();
            let observability = self.inner.observability.clone();
            let blocking_shutdown_timeout = self.inner.blocking_shutdown_timeout;
            let max_blocking_tasks_per_actor = self.inner.max_blocking_tasks_per_actor;
            let actor_shutdown = actor_shutdown.clone();

            async move {
                let mut blocking = BlockingRuntime::new(
                    Arc::clone(&actor_id),
                    myself.clone(),
                    actor_shutdown.clone(),
                    observability.clone(),
                    max_blocking_tasks_per_actor,
                    blocking_shutdown_timeout,
                );
                let ctx = ActorContext {
                    id: Arc::clone(&actor_id),
                    mailbox: receiver,
                    peers,
                    registry,
                    myself,
                    shutdown: actor_shutdown.clone(),
                    blocking: blocking.spawner(),
                    observability: observability.clone(),
                };
                let actor_future = spec.factory.make(ctx);
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

                blocking.finish(result).await
            }
            .instrument(actor_span)
        }));
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

        drop(bound_mailbox);

        match result {
            Ok(Ok(())) => {
                let status = if actor_shutdown.is_cancelled() {
                    ActorExitStatus::Shutdown
                } else {
                    ActorExitStatus::Stopped
                };
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
                self.inner.observability.emit_actor_exited(
                    &actor_id,
                    ActorExitStatus::Failed,
                    Some(&error.to_string()),
                );
                Err(error)
            }
            Err(err) if err.is_panic() => {
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
                self.inner.observability.emit_actor_exited(
                    &actor_id,
                    ActorExitStatus::Cancelled,
                    Some(&error.to_string()),
                );
                Err(error)
            }
        }
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
    /// Constructs a runtime actor with no statically-linked peers.
    pub fn actor(&self, spec: ActorSpec) -> RunnableActor {
        self.actor_with_peers(spec, HashMap::new())
    }

    /// Constructs a runtime actor whose initial peer map is resolved from the
    /// provided registry.
    pub fn actor_with_peer_ids<I, S>(
        &self,
        spec: ActorSpec,
        registry: &ActorRegistry,
        peer_ids: I,
    ) -> Result<RunnableActor, RegistryError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let actor_id = Arc::clone(&spec.inner.id);
        let mut peers = HashMap::new();

        for peer_id in peer_ids {
            let peer_id = peer_id.as_ref();
            let peer = registry
                .actor_ref_for_source(peer_id, Some(&actor_id))
                .ok_or_else(|| RegistryError::UnknownActorId(peer_id.to_owned()))?;
            peers.insert(peer_id.into(), peer);
        }

        Ok(self.actor_with_peers(spec, peers))
    }

    fn actor_with_peers(
        &self,
        spec: ActorSpec,
        peers: HashMap<Arc<str>, ActorRef>,
    ) -> RunnableActor {
        let actor_id = Arc::clone(&spec.inner.id);
        let binding = Arc::new(MailboxBinding::default());
        let observability = self.observability.clone();
        let myself = ActorRef::from_binding(
            Arc::clone(&actor_id),
            binding.subscribe(),
            observability.clone(),
            Some(Arc::clone(&actor_id)),
        );

        RunnableActor {
            inner: Arc::new(RunnableActorInner {
                spec: Arc::clone(&spec.inner),
                binding,
                peers,
                myself,
                registry: OnceLock::new(),
                mailbox_capacity: self.mailbox_capacity,
                max_envelope_bytes: self.max_envelope_bytes,
                max_blocking_tasks_per_actor: self.max_blocking_tasks_per_actor,
                blocking_shutdown_timeout: self.blocking_shutdown_timeout,
                observability,
                ingress_names: Vec::new(),
                running: AtomicBool::new(false),
            }),
        }
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
                actor_id: inner.spec.id.to_string(),
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

fn peer_refs(
    graph: &Arc<GraphInner>,
    actor_id: &Arc<str>,
) -> Result<HashMap<Arc<str>, ActorRef>, GraphError> {
    let linked_peers = graph
        .links
        .get(actor_id)
        .ok_or_else(|| GraphError::InvalidState {
            detail: format!("actor `{actor_id}` is missing its link definition"),
        })?;
    let mut peers = HashMap::with_capacity(linked_peers.len());

    for peer_id in linked_peers {
        let binding = graph.actor_binding(peer_id)?;
        peers.insert(
            Arc::clone(peer_id),
            ActorRef::from_binding(
                Arc::clone(peer_id),
                binding.subscribe(),
                graph.observability.clone(),
                Some(Arc::clone(actor_id)),
            ),
        );
    }

    Ok(peers)
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
