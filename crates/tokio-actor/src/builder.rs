use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    marker::PhantomData,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::{
    actor::RawActor,
    binding::{BindingCore, BindingLifecycle},
    context::ActorRef,
    error::GraphBuildError,
    graph::{ErasedRunner, Graph, GraphActor, GraphInner, TypedRunner},
    observability::{GraphObservability, anonymous_graph_name},
};

static NEXT_BUILDER_ID: AtomicU64 = AtomicU64::new(1);

/// Unfilled position for one actor in a graph builder.
///
/// Slots are created by [`GraphBuilder::slot`] and consumed by
/// [`GraphBuilder::define`]. The token is intentionally neither [`Clone`] nor
/// [`Copy`], so a slot can only be filled once in ordinary Rust code.
pub struct ActorSlot<M> {
    builder_id: u64,
    index: Option<usize>,
    _message: PhantomData<fn(M)>,
}

impl<M> ActorSlot<M> {
    fn new(builder_id: u64, index: Option<usize>) -> Self {
        Self {
            builder_id,
            index,
            _message: PhantomData,
        }
    }
}

/// Builder for constructing a validated actor graph.
///
/// Registration methods mint restart-stable, typed [`ActorRef`]s immediately,
/// so refs can be captured by other actors' state. Cyclic graphs use
/// [`slot`](Self::slot) to create refs first and [`define`](Self::define) to
/// fill the slots once actor values have been wired.
pub struct GraphBuilder {
    builder_id: u64,
    name: Option<String>,
    slots: Vec<Slot>,
    index: HashMap<Arc<str>, usize>,
    errors: Vec<GraphBuildError>,
    mailbox_capacity: usize,
    actor_shutdown_timeout: Duration,
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
pub(crate) const DEFAULT_ACTOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
            builder_id: NEXT_BUILDER_ID.fetch_add(1, Ordering::Relaxed),
            name: None,
            slots: Vec::new(),
            index: HashMap::new(),
            errors: Vec::new(),
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            actor_shutdown_timeout: DEFAULT_ACTOR_SHUTDOWN_TIMEOUT,
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

    /// Sets how long shutdown waits for an actor task to stop after
    /// cancellation is requested.
    ///
    /// This applies to [`Graph::run_until`](crate::Graph::run_until) and to
    /// each [`RunnableActor::run_until`](crate::RunnableActor::run_until)
    /// independently. The default timeout is 5 seconds. Any actor task still
    /// running after the timeout is aborted; when this happens during a
    /// requested shutdown it is reported as a clean shutdown with a
    /// `Cancelled` actor exit.
    pub fn actor_shutdown_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.actor_shutdown_timeout = timeout;
        self
    }

    /// Opens a named slot and returns its fill token plus a restart-stable ref.
    ///
    /// This enables cyclic wiring: create all refs first, hand them to actor
    /// constructors, then consume each [`ActorSlot`] with [`define`](Self::define).
    /// The name is fixed when the slot is opened because it is used as the
    /// actor id in observability and runtime lookups.
    pub fn slot<M: Send + 'static>(&mut self, actor_id: &str) -> (ActorSlot<M>, ActorRef<M>) {
        match self.push_slot::<M>(actor_id) {
            Some((index, actor_ref)) => (ActorSlot::new(self.builder_id, Some(index)), actor_ref),
            None => (
                ActorSlot::new(self.builder_id, None),
                ActorRef::detached(actor_id.into()),
            ),
        }
    }

    /// Fills a previously opened actor slot.
    ///
    /// The slot token's message type must match the actor's message type, so a
    /// mismatched actor is rejected by the compiler. Consuming the token makes
    /// double fills unrepresentable in ordinary Rust code.
    pub fn define<A: RawActor>(&mut self, slot: ActorSlot<A::Msg>, actor: A) {
        if slot.builder_id != self.builder_id {
            self.errors.push(GraphBuildError::InvalidConfig(
                "actor slot belongs to a different graph builder",
            ));
            return;
        }

        let Some(index) = slot.index else {
            return;
        };
        let Some(slot) = self.slots.get_mut(index) else {
            self.errors
                .push(GraphBuildError::InvalidConfig("actor slot is detached"));
            return;
        };

        debug_assert_eq!(slot.message_type, TypeId::of::<A::Msg>());
        let Ok(binding) = slot.binding.clone().downcast::<BindingCore<A::Msg>>() else {
            unreachable!("message type enforced by ActorSlot")
        };
        slot.runner = Some(Arc::new(TypedRunner { actor, binding }));
    }

    /// Registers an actor and returns its typed, restart-stable ref.
    pub fn actor<A: RawActor>(&mut self, actor_id: &str, actor: A) -> ActorRef<A::Msg> {
        let Some((index, actor_ref)) = self.push_slot::<A::Msg>(actor_id) else {
            return ActorRef::detached(actor_id.into());
        };
        let slot = &mut self.slots[index];
        let Ok(binding) = slot.binding.clone().downcast::<BindingCore<A::Msg>>() else {
            unreachable!("message type id already verified")
        };
        slot.runner = Some(Arc::new(TypedRunner { actor, binding }));
        actor_ref
    }

    /// Registers an actor under its unqualified type name.
    ///
    /// If multiple actors have the same type name, later registrations receive
    /// `-2`, `-3`, and so on. Renaming the actor type therefore renames tracing
    /// fields and metric labels; users who need stable observability names
    /// should use [`actor`](Self::actor) or `#[derive(Topology)]` field names.
    pub fn add<A: RawActor>(&mut self, actor: A) -> ActorRef<A::Msg> {
        let base = short_type_name(type_name::<A>());
        let mut actor_id = base.to_owned();
        let mut suffix = 2;
        while self.index.contains_key(actor_id.as_str()) {
            actor_id = format!("{base}-{suffix}");
            suffix += 1;
        }
        self.actor(&actor_id, actor)
    }

    /// Validates the graph and returns an immutable [`Graph`].
    pub fn build(mut self) -> Result<Graph, GraphBuildError> {
        let graph_name = match self.name {
            Some(name) if name.is_empty() => {
                return Err(GraphBuildError::InvalidConfig(
                    "graph name must not be empty",
                ));
            }
            Some(name) => Arc::from(name),
            None => anonymous_graph_name(),
        };

        if !self.errors.is_empty() {
            return Err(self.errors.remove(0));
        }
        if self.slots.is_empty() {
            return Err(GraphBuildError::EmptyGraph);
        }
        if self.mailbox_capacity == 0 {
            return Err(GraphBuildError::InvalidConfig(
                "mailbox capacity must be non-zero",
            ));
        }

        let observability = GraphObservability::new(Arc::clone(&graph_name));
        let mut actors = Vec::with_capacity(self.slots.len());

        for slot in self.slots {
            let Some(runner) = slot.runner else {
                return Err(GraphBuildError::MissingActor {
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
            actor_shutdown_timeout: self.actor_shutdown_timeout,
            state: AtomicU8::new(0),
            observability,
        }))
    }

    /// Creates a named slot and returns its index plus typed ref.
    fn push_slot<M: Send + 'static>(&mut self, actor_id: &str) -> Option<(usize, ActorRef<M>)> {
        if actor_id.is_empty() {
            self.errors
                .push(GraphBuildError::InvalidConfig("actor id must not be empty"));
            return None;
        }

        if self.index.contains_key(actor_id) {
            self.errors.push(GraphBuildError::DuplicateActorId {
                actor_id: actor_id.to_string(),
            });
            return None;
        }

        let actor_id: Arc<str> = actor_id.into();
        let core = Arc::new(BindingCore::<M>::new(actor_id.clone()));
        let observability = core.observability_slot();
        let actor_ref = ActorRef::from_core(&core, None);
        let index = self.slots.len();
        self.index.insert(actor_id.clone(), index);
        self.slots.push(Slot {
            actor_id,
            message_type: TypeId::of::<M>(),
            message_type_name: type_name::<M>(),
            binding: core.clone(),
            binding_lifecycle: core,
            observability,
            runner: None,
        });
        Some((index, actor_ref))
    }
}

fn short_type_name(type_name: &str) -> &str {
    let name = type_name
        .split_once('<')
        .map_or(type_name, |(name, _)| name);
    name.rsplit("::").next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::short_type_name;

    #[test]
    fn short_type_name_handles_plain_path_qualified_and_generics() {
        assert_eq!(short_type_name("Sink"), "Sink");
        assert_eq!(short_type_name("orders::Gateway"), "Gateway");
        assert_eq!(short_type_name("orders::Gateway<fix::Fix>"), "Gateway");
        assert_eq!(
            short_type_name("orders::Gateway<fix::Fix<wire::Header>, sink::Out>"),
            "Gateway"
        );
    }
}
