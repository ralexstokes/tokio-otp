use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::AtomicBool},
};

use crate::{
    actor::ActorSpec,
    error::BuildError,
    graph::{Graph, GraphInner, IngressDefinition},
    ingress::IngressBinding,
    observability::{GraphObservability, anonymous_graph_name},
};

/// Builder for constructing a validated actor graph.
///
/// Graphs are static in this first iteration: actors, links, and ingress
/// points are all defined up front before calling [`build`](Self::build).
pub struct GraphBuilder {
    name: Option<String>,
    actors: Vec<ActorSpec>,
    links: Vec<(String, String)>,
    ingresses: Vec<(String, String)>,
    mailbox_capacity: usize,
}

const DEFAULT_MAILBOX_CAPACITY: usize = 64;

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
            actors: Vec::new(),
            links: Vec::new(),
            ingresses: Vec::new(),
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
        }
    }

    /// Sets the graph name used in tracing fields and metric labels.
    ///
    /// If omitted, a stable anonymous name is generated during
    /// [`build`](Self::build).
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Appends an actor to the graph.
    #[must_use]
    pub fn actor(mut self, actor: ActorSpec) -> Self {
        self.actors.push(actor);
        self
    }

    /// Adds a directed link from `from` to `to`.
    ///
    /// Actors only receive [`ActorRef`](crate::ActorRef) handles for peers
    /// that are linked from the actor.
    #[must_use]
    pub fn link(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.links.push((from.into(), to.into()));
        self
    }

    /// Adds a named stable ingress point that routes external messages to the
    /// target actor.
    #[must_use]
    pub fn ingress(mut self, name: impl Into<String>, target_actor: impl Into<String>) -> Self {
        self.ingresses.push((name.into(), target_actor.into()));
        self
    }

    /// Sets the bounded mailbox capacity used for every actor in the graph.
    #[must_use]
    pub fn mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    /// Validates the graph and returns an immutable [`Graph`].
    pub fn build(self) -> Result<Graph, BuildError> {
        let graph_name = match self.name {
            Some(name) if name.is_empty() => {
                return Err(BuildError::InvalidConfig("graph name must not be empty"));
            }
            Some(name) => name.into(),
            None => anonymous_graph_name(),
        };

        if self.actors.is_empty() {
            return Err(BuildError::EmptyActors);
        }

        if self.mailbox_capacity == 0 {
            return Err(BuildError::InvalidConfig(
                "mailbox capacity must be non-zero",
            ));
        }

        let mut actor_ids = HashSet::new();
        let mut actors = Vec::with_capacity(self.actors.len());
        let mut links: HashMap<Arc<str>, Vec<Arc<str>>> = HashMap::new();

        for actor in self.actors {
            if actor.id().is_empty() {
                return Err(BuildError::InvalidConfig("actor id must not be empty"));
            }
            if !actor_ids.insert(actor.id().to_owned()) {
                return Err(BuildError::DuplicateActorId(actor.id().to_owned()));
            }
            links.insert(Arc::clone(&actor.inner.id), Vec::new());
            actors.push(Arc::clone(&actor.inner));
        }

        for (from, to) in self.links {
            if !actor_ids.contains(&from) {
                return Err(BuildError::UnknownLinkSource { actor: from });
            }
            if !actor_ids.contains(&to) {
                return Err(BuildError::UnknownLinkTarget { from, actor: to });
            }
            let outgoing = links
                .get_mut(from.as_str())
                .expect("links entry guaranteed by actor_ids check");
            outgoing.push(to.into());
        }

        let mut ingress_names = HashSet::new();
        let mut ingresses: HashMap<Arc<str>, IngressDefinition> = HashMap::new();
        for (name, target) in self.ingresses {
            if name.is_empty() {
                return Err(BuildError::InvalidConfig("ingress name must not be empty"));
            }
            if !ingress_names.insert(name.clone()) {
                return Err(BuildError::DuplicateIngressName(name));
            }
            if !actor_ids.contains(&target) {
                return Err(BuildError::UnknownIngressTarget {
                    ingress: name,
                    actor: target,
                });
            }

            ingresses.insert(
                name.into(),
                IngressDefinition {
                    target_actor: target.into(),
                    binding: Arc::new(IngressBinding::default()),
                },
            );
        }

        Ok(Graph::new(GraphInner {
            actors,
            links,
            mailbox_capacity: self.mailbox_capacity,
            ingresses,
            running: AtomicBool::new(false),
            observability: GraphObservability::new(graph_name),
        }))
    }
}
