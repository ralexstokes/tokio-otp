use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    sync::{Arc, OnceLock, atomic::AtomicU8},
    time::Duration,
};

use crate::{
    actor::Actor,
    binding::{BindingCore, BindingLifecycle},
    context::ActorRef,
    error::BuildError,
    graph::{ErasedRunner, Graph, GraphActor, GraphInner, TypedRunner},
    observability::{GraphObservability, anonymous_graph_name},
};

/// Builder for constructing a validated actor graph.
///
/// Registration methods mint restart-stable, typed [`ActorRef`]s immediately,
/// so refs can be captured by other actors' state, including cyclically via
/// [`declare`](Self::declare).
pub struct GraphBuilder {
    name: Option<String>,
    slots: Vec<Slot>,
    index: HashMap<Arc<str>, usize>,
    errors: Vec<BuildError>,
    mailbox_capacity: usize,
    max_blocking_tasks_per_actor: Option<usize>,
    actor_shutdown_timeout: Duration,
    blocking_shutdown_timeout: Duration,
}

struct Slot {
    actor_id: Arc<str>,
    message_type: TypeId,
    message_type_name: &'static str,
    binding: Arc<dyn Any + Send + Sync>,
    binding_lifecycle: Arc<dyn BindingLifecycle>,
    observability: Arc<OnceLock<GraphObservability>>,
    runner: Option<Arc<dyn ErasedRunner>>,
}

pub(crate) const DEFAULT_MAILBOX_CAPACITY: usize = 64;
pub(crate) const DEFAULT_MAX_BLOCKING_TASKS_PER_ACTOR: usize = 16;
pub(crate) const DEFAULT_BLOCKING_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

impl Default for GraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphBuilder {
    /// Creates a new builder with no actors and a default mailbox capacity of
    /// 64 messages per actor.
    pub fn new() -> Self {
        Self {
            name: None,
            slots: Vec::new(),
            index: HashMap::new(),
            errors: Vec::new(),
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            max_blocking_tasks_per_actor: Some(DEFAULT_MAX_BLOCKING_TASKS_PER_ACTOR),
            actor_shutdown_timeout: DEFAULT_BLOCKING_SHUTDOWN_TIMEOUT,
            blocking_shutdown_timeout: DEFAULT_BLOCKING_SHUTDOWN_TIMEOUT,
        }
    }

    /// Sets the graph name used in tracing fields and metric labels.
    ///
    /// If omitted, a stable anonymous name is generated during
    /// [`build`](Self::build).
    pub fn name(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the bounded mailbox capacity used for every actor in the graph.
    pub fn mailbox_capacity(&mut self, capacity: usize) -> &mut Self {
        self.mailbox_capacity = capacity;
        self
    }

    /// Sets the maximum number of active blocking tasks allowed per actor.
    ///
    /// The default limit is 16 active tasks. New blocking work is rejected
    /// once the actor reaches the configured limit.
    pub fn max_blocking_tasks_per_actor(&mut self, limit: usize) -> &mut Self {
        self.max_blocking_tasks_per_actor = Some(limit);
        self
    }

    /// Disables the per-actor blocking task concurrency limit.
    pub fn unbounded_blocking_tasks_per_actor(&mut self) -> &mut Self {
        self.max_blocking_tasks_per_actor = None;
        self
    }

    /// Sets how long graph shutdown waits for actor tasks to stop after
    /// cancellation is requested.
    ///
    /// The default timeout is 5 seconds. Any actor task still running after the
    /// timeout is aborted so graph shutdown can complete.
    pub fn actor_shutdown_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.actor_shutdown_timeout = timeout;
        self
    }

    /// Sets how long shutdown waits for blocking tasks to stop after
    /// cancellation is requested.
    ///
    /// The default timeout is 5 seconds. Any blocking task still running after
    /// the timeout is detached so the graph can terminate.
    pub fn blocking_shutdown_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.blocking_shutdown_timeout = timeout;
        self
    }

    /// Mints a typed ref for an actor that will be registered later.
    ///
    /// This enables cyclic wiring: declare an actor, hand its ref to another
    /// actor's state, then register the declared actor.
    pub fn declare<M: Send + 'static>(&mut self, actor_id: &str) -> ActorRef<M> {
        self.slot_ref::<M>(actor_id)
    }

    /// Registers an actor and returns its typed, restart-stable ref.
    pub fn actor<A: Actor>(&mut self, actor_id: &str, actor: A) -> ActorRef<A::Msg> {
        if actor_id.is_empty() {
            self.errors
                .push(BuildError::InvalidConfig("actor id must not be empty"));
            return ActorRef::detached(actor_id.into());
        }

        if let Some(&index) = self.index.get(actor_id) {
            let slot = &self.slots[index];
            if slot.message_type == TypeId::of::<A::Msg>() && slot.runner.is_some() {
                self.errors.push(BuildError::DuplicateActorId {
                    actor_id: actor_id.to_string(),
                });
                return ActorRef::detached(actor_id.into());
            }
        }

        let actor_ref = self.slot_ref::<A::Msg>(actor_id);
        let Some(&index) = self.index.get(actor_id) else {
            return actor_ref;
        };
        let slot = &mut self.slots[index];
        if slot.message_type != TypeId::of::<A::Msg>() {
            return actor_ref;
        }

        let Ok(binding) = slot.binding.clone().downcast::<BindingCore<A::Msg>>() else {
            unreachable!("message type id already verified")
        };
        slot.runner = Some(Arc::new(TypedRunner { actor, binding }));
        actor_ref
    }

    /// Validates the graph and returns an immutable [`Graph`].
    pub fn build(mut self) -> Result<Graph, BuildError> {
        let graph_name = match self.name {
            Some(name) if name.is_empty() => {
                return Err(BuildError::InvalidConfig("graph name must not be empty"));
            }
            Some(name) => Arc::from(name),
            None => anonymous_graph_name(),
        };

        if !self.errors.is_empty() {
            return Err(self.errors.remove(0));
        }
        if self.slots.is_empty() {
            return Err(BuildError::EmptyGraph);
        }
        if self.mailbox_capacity == 0 {
            return Err(BuildError::InvalidConfig(
                "mailbox capacity must be non-zero",
            ));
        }

        let observability = GraphObservability::new(Arc::clone(&graph_name));
        let mut actors = Vec::with_capacity(self.slots.len());

        for slot in self.slots {
            let Some(runner) = slot.runner else {
                return Err(BuildError::MissingActor {
                    actor_id: slot.actor_id.to_string(),
                });
            };
            let _ = slot.observability.set(observability.clone());
            actors.push(GraphActor {
                actor_id: slot.actor_id,
                message_type: slot.message_type,
                message_type_name: slot.message_type_name,
                binding: slot.binding,
                binding_lifecycle: slot.binding_lifecycle,
                observability: slot.observability,
                runner,
            });
        }

        Ok(Graph::new(GraphInner {
            name: graph_name,
            actors,
            actor_index: self.index,
            mailbox_capacity: self.mailbox_capacity,
            max_blocking_tasks_per_actor: self.max_blocking_tasks_per_actor,
            actor_shutdown_timeout: self.actor_shutdown_timeout,
            blocking_shutdown_timeout: self.blocking_shutdown_timeout,
            state: AtomicU8::new(0),
            observability,
        }))
    }

    /// Returns a typed ref bound to the slot for `actor_id`, creating the slot
    /// if needed. On a message-type mismatch, records the error and returns a
    /// detached ref that never binds.
    fn slot_ref<M: Send + 'static>(&mut self, actor_id: &str) -> ActorRef<M> {
        if actor_id.is_empty() {
            self.errors
                .push(BuildError::InvalidConfig("actor id must not be empty"));
            return ActorRef::detached(actor_id.into());
        }

        if let Some(&index) = self.index.get(actor_id) {
            let slot = &self.slots[index];
            if slot.message_type == TypeId::of::<M>() {
                let Ok(core) = slot.binding.clone().downcast::<BindingCore<M>>() else {
                    unreachable!("message type id already verified")
                };
                return ActorRef::from_core(&core, None);
            }
            self.errors.push(BuildError::MessageTypeMismatch {
                actor_id: actor_id.to_string(),
                registered: slot.message_type_name,
                requested: type_name::<M>(),
            });
            return ActorRef::detached(actor_id.into());
        }

        let actor_id: Arc<str> = actor_id.into();
        let core = Arc::new(BindingCore::<M>::new(actor_id.clone()));
        let observability = core.observability_slot();
        let actor_ref = ActorRef::from_core(&core, None);
        self.index.insert(actor_id.clone(), self.slots.len());
        self.slots.push(Slot {
            actor_id,
            message_type: TypeId::of::<M>(),
            message_type_name: type_name::<M>(),
            binding: core.clone(),
            binding_lifecycle: core,
            observability,
            runner: None,
        });
        actor_ref
    }
}
