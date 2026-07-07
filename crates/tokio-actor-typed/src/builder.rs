use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    sync::Arc,
};

use crate::{
    actor::Actor,
    binding::BindingCore,
    context::ActorRef,
    error::BuildError,
    graph::{ErasedRunner, Graph, GraphActor, TypedRunner},
};

const DEFAULT_MAILBOX_CAPACITY: usize = 64;

struct Slot {
    actor_id: Arc<str>,
    message_type: TypeId,
    message_type_name: &'static str,
    binding: Arc<dyn Any + Send + Sync>,
    runner: Option<Arc<dyn ErasedRunner>>,
}

/// Constructs and validates a typed actor graph.
///
/// Registration methods mint restart-stable, typed [`ActorRef`]s
/// immediately, so refs can be captured by other actors' state — including
/// cyclically, via [`declare`](Self::declare). Errors (duplicate ids,
/// message-type mismatches, declared-but-missing actors) are collected and
/// reported by [`build`](Self::build).
pub struct GraphBuilder {
    name: Arc<str>,
    slots: Vec<Slot>,
    index: HashMap<Arc<str>, usize>,
    errors: Vec<BuildError>,
    mailbox_capacity: usize,
}

impl Default for GraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphBuilder {
    /// Creates a new builder with default settings.
    pub fn new() -> Self {
        Self {
            name: "graph".into(),
            slots: Vec::new(),
            index: HashMap::new(),
            errors: Vec::new(),
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
        }
    }

    /// Sets the graph name.
    pub fn name(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = name.into().into();
        self
    }

    /// Sets the per-actor mailbox capacity (default 64, minimum 1).
    pub fn mailbox_capacity(&mut self, capacity: usize) -> &mut Self {
        self.mailbox_capacity = capacity.max(1);
        self
    }

    /// Mints a typed ref for an actor that will be registered later.
    ///
    /// This enables cyclic wiring: declare an actor, hand its ref to another
    /// actor's state, then register the declared actor.
    /// [`build`](Self::build) fails with [`BuildError::MissingActor`] if a
    /// declared actor is never registered, and with
    /// [`BuildError::MessageTypeMismatch`] if it is registered with a
    /// different message type.
    pub fn declare<M: Send + 'static>(&mut self, actor_id: &str) -> ActorRef<M> {
        self.slot_ref::<M>(actor_id)
    }

    /// Registers an actor and returns its typed, restart-stable ref.
    pub fn actor<A: Actor>(&mut self, actor_id: &str, actor: A) -> ActorRef<A::Msg> {
        let actor_ref = self.slot_ref::<A::Msg>(actor_id);
        let index = self.index[actor_id];
        let slot = &mut self.slots[index];
        if slot.message_type != TypeId::of::<A::Msg>() {
            // Mismatch already recorded by slot_ref.
            return actor_ref;
        }
        if slot.runner.is_some() {
            self.errors.push(BuildError::DuplicateActor {
                actor_id: actor_id.to_string(),
            });
            return actor_ref;
        }
        let Ok(binding) = slot.binding.clone().downcast::<BindingCore<A::Msg>>() else {
            unreachable!("message type id already verified")
        };
        slot.runner = Some(Arc::new(TypedRunner { actor, binding }));
        actor_ref
    }

    /// Validates the graph and returns the immutable, rerunnable spec.
    pub fn build(mut self) -> Result<Graph, BuildError> {
        if !self.errors.is_empty() {
            return Err(self.errors.remove(0));
        }
        if self.slots.is_empty() {
            return Err(BuildError::EmptyGraph);
        }

        let mut actors = Vec::with_capacity(self.slots.len());
        for slot in self.slots {
            let Some(runner) = slot.runner else {
                return Err(BuildError::MissingActor {
                    actor_id: slot.actor_id.to_string(),
                });
            };
            actors.push(GraphActor {
                actor_id: slot.actor_id,
                message_type: slot.message_type,
                message_type_name: slot.message_type_name,
                binding: slot.binding,
                runner,
            });
        }

        Ok(Graph::new(
            self.name,
            self.mailbox_capacity,
            actors,
            self.index,
        ))
    }

    /// Returns a typed ref bound to the slot for `actor_id`, creating the
    /// slot if needed. On a message-type mismatch, records the error and
    /// returns a detached ref that never binds.
    fn slot_ref<M: Send + 'static>(&mut self, actor_id: &str) -> ActorRef<M> {
        if let Some(&index) = self.index.get(actor_id) {
            let slot = &self.slots[index];
            if slot.message_type == TypeId::of::<M>() {
                let Ok(core) = slot.binding.clone().downcast::<BindingCore<M>>() else {
                    unreachable!("message type id already verified")
                };
                return ActorRef::new(slot.actor_id.clone(), core.subscribe());
            }
            self.errors.push(BuildError::MessageTypeMismatch {
                actor_id: actor_id.to_string(),
                registered: slot.message_type_name,
                requested: type_name::<M>(),
            });
            let detached = BindingCore::<M>::new(actor_id.into());
            return ActorRef::new(detached.actor_id().clone(), detached.subscribe());
        }

        let actor_id: Arc<str> = actor_id.into();
        let core = Arc::new(BindingCore::<M>::new(actor_id.clone()));
        let actor_ref = ActorRef::new(actor_id.clone(), core.subscribe());
        self.index.insert(actor_id.clone(), self.slots.len());
        self.slots.push(Slot {
            actor_id,
            message_type: TypeId::of::<M>(),
            message_type_name: type_name::<M>(),
            binding: core,
            runner: None,
        });
        actor_ref
    }
}
