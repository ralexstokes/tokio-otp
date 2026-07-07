use std::{
    any::{Any, TypeId},
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Arc,
};

use tokio::{
    sync::mpsc,
    task::{Id, JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;

use crate::{
    actor::{Actor, ActorResult},
    binding::{BindingCore, BindingGuard, MailboxRef},
    context::{ActorContext, ActorRef},
    error::{GraphError, LookupError},
};

pub(crate) type BoxedActorFuture = Pin<Box<dyn Future<Output = ActorResult> + Send + 'static>>;

/// Type-erased actor runner.
///
/// This is the only dyn layer in the crate: each implementation knows its own
/// message type and owns the typed binding core, so starting an actor binds a
/// typed mailbox without any downcast.
pub(crate) trait ErasedRunner: Send + Sync {
    fn start(&self, shutdown: CancellationToken, mailbox_capacity: usize) -> BoxedActorFuture;
}

pub(crate) struct TypedRunner<A: Actor> {
    pub(crate) actor: A,
    pub(crate) binding: Arc<BindingCore<A::Msg>>,
}

impl<A: Actor> ErasedRunner for TypedRunner<A> {
    fn start(&self, shutdown: CancellationToken, mailbox_capacity: usize) -> BoxedActorFuture {
        let (sender, mailbox) = mpsc::channel(mailbox_capacity);
        let actor_id = self.binding.actor_id().clone();
        let guard = BindingGuard::bind(
            self.binding.clone(),
            MailboxRef::new(actor_id.clone(), sender),
        );
        let myself = ActorRef::new(actor_id.clone(), self.binding.subscribe());
        let ctx = ActorContext {
            id: actor_id,
            mailbox,
            shutdown,
            myself,
        };
        let actor = self.actor.clone();
        Box::pin(async move {
            let _guard = guard;
            actor.run(ctx).await
        })
    }
}

pub(crate) struct GraphActor {
    pub(crate) actor_id: Arc<str>,
    pub(crate) message_type: TypeId,
    pub(crate) message_type_name: &'static str,
    pub(crate) binding: Arc<dyn Any + Send + Sync>,
    pub(crate) runner: Arc<dyn ErasedRunner>,
}

struct GraphInner {
    name: Arc<str>,
    mailbox_capacity: usize,
    actors: Vec<GraphActor>,
    index: HashMap<Arc<str>, usize>,
}

/// Immutable, cloneable graph spec that can be rerun.
///
/// Typed [`ActorRef`]s minted by the builder (or via [`Graph::actor_ref`])
/// stay valid across reruns of the same graph.
#[derive(Clone)]
pub struct Graph {
    inner: Arc<GraphInner>,
}

impl Graph {
    pub(crate) fn new(
        name: Arc<str>,
        mailbox_capacity: usize,
        actors: Vec<GraphActor>,
        index: HashMap<Arc<str>, usize>,
    ) -> Self {
        Self {
            inner: Arc::new(GraphInner {
                name,
                mailbox_capacity,
                actors,
                index,
            }),
        }
    }

    /// Returns the graph name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Returns a typed ref for a graph actor, checked at runtime.
    ///
    /// This is the registry-style dynamic lookup: the message type is
    /// verified against the type the actor was registered with.
    pub fn actor_ref<M: Send + 'static>(&self, actor_id: &str) -> Result<ActorRef<M>, LookupError> {
        let Some(&index) = self.inner.index.get(actor_id) else {
            return Err(LookupError::UnknownActor {
                actor_id: actor_id.to_string(),
            });
        };
        let actor = &self.inner.actors[index];
        if actor.message_type != TypeId::of::<M>() {
            return Err(LookupError::MessageTypeMismatch {
                actor_id: actor_id.to_string(),
                registered: actor.message_type_name,
                requested: std::any::type_name::<M>(),
            });
        }
        let Ok(core) = actor.binding.clone().downcast::<BindingCore<M>>() else {
            unreachable!("message type id already verified")
        };
        Ok(ActorRef::new(actor.actor_id.clone(), core.subscribe()))
    }

    /// Runs the graph until the shutdown future completes or an actor stops.
    ///
    /// Mirrors `tokio-actor` semantics: an actor that fails, panics, or exits
    /// cleanly before shutdown fails the whole run. After shutdown is
    /// requested, clean exits are expected and only failures are reported.
    pub async fn run_until<F: Future>(&self, shutdown: F) -> Result<(), GraphError> {
        let token = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let mut task_ids = HashMap::new();

        for actor in &self.inner.actors {
            let future = actor
                .runner
                .start(token.clone(), self.inner.mailbox_capacity);
            let handle = tasks.spawn(future);
            task_ids.insert(handle.id(), actor.actor_id.clone());
        }

        let mut first_error = None;

        tokio::select! {
            _ = shutdown => {}
            joined = tasks.join_next_with_id() => {
                if let Some(result) = joined {
                    first_error = Some(match result {
                        Ok((task_id, Ok(()))) => GraphError::ActorExitedEarly {
                            actor_id: actor_id_for(&task_ids, task_id),
                        },
                        Ok((task_id, Err(source))) => GraphError::ActorFailed {
                            actor_id: actor_id_for(&task_ids, task_id),
                            source,
                        },
                        Err(join_error) => join_failure(&task_ids, join_error),
                    });
                }
            }
        }

        token.cancel();

        while let Some(result) = tasks.join_next_with_id().await {
            if first_error.is_some() {
                continue;
            }
            first_error = match result {
                Ok((_, Ok(()))) => None,
                Ok((task_id, Err(source))) => Some(GraphError::ActorFailed {
                    actor_id: actor_id_for(&task_ids, task_id),
                    source,
                }),
                Err(join_error) if join_error.is_panic() => Some(GraphError::ActorPanicked {
                    actor_id: actor_id_for(&task_ids, join_error.id()),
                }),
                Err(_) => None,
            };
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn actor_id_for(task_ids: &HashMap<Id, Arc<str>>, task_id: Id) -> String {
    task_ids
        .get(&task_id)
        .map_or_else(|| "<unknown>".to_string(), |actor_id| actor_id.to_string())
}

fn join_failure(task_ids: &HashMap<Id, Arc<str>>, join_error: JoinError) -> GraphError {
    let actor_id = actor_id_for(task_ids, join_error.id());
    if join_error.is_panic() {
        GraphError::ActorPanicked { actor_id }
    } else {
        GraphError::ActorFailed {
            actor_id,
            source: Box::new(join_error),
        }
    }
}
