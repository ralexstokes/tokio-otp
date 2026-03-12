use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
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
    max_envelope_bytes: Option<usize>,
    max_blocking_tasks_per_actor: Option<usize>,
    blocking_shutdown_timeout: Duration,
}

const DEFAULT_MAILBOX_CAPACITY: usize = 64;
const DEFAULT_MAX_ENVELOPE_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_BLOCKING_TASKS_PER_ACTOR: usize = 16;
const DEFAULT_BLOCKING_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
            max_envelope_bytes: Some(DEFAULT_MAX_ENVELOPE_BYTES),
            max_blocking_tasks_per_actor: Some(DEFAULT_MAX_BLOCKING_TASKS_PER_ACTOR),
            blocking_shutdown_timeout: DEFAULT_BLOCKING_SHUTDOWN_TIMEOUT,
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

    /// Sets the maximum envelope size accepted by actor mailboxes.
    ///
    /// The default limit is 1 MiB. Messages that exceed the limit are rejected
    /// at send time before they enter a mailbox.
    #[must_use]
    pub fn max_envelope_bytes(mut self, max_bytes: usize) -> Self {
        self.max_envelope_bytes = Some(max_bytes);
        self
    }

    /// Disables the mailbox envelope size limit.
    #[must_use]
    pub fn disable_envelope_size_limit(mut self) -> Self {
        self.max_envelope_bytes = None;
        self
    }

    /// Sets the maximum number of active blocking tasks allowed per actor.
    ///
    /// The default limit is 16 active tasks. New blocking work is rejected
    /// once the actor reaches the configured limit.
    #[must_use]
    pub fn max_blocking_tasks_per_actor(mut self, limit: usize) -> Self {
        self.max_blocking_tasks_per_actor = Some(limit);
        self
    }

    /// Disables the per-actor blocking task concurrency limit.
    #[must_use]
    pub fn unbounded_blocking_tasks_per_actor(mut self) -> Self {
        self.max_blocking_tasks_per_actor = None;
        self
    }

    /// Sets how long shutdown waits for blocking tasks to stop after
    /// cancellation is requested.
    ///
    /// The default timeout is 5 seconds. Any blocking task still running after
    /// the timeout is detached so graph shutdown can complete.
    #[must_use]
    pub fn blocking_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.blocking_shutdown_timeout = timeout;
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
            max_envelope_bytes: self.max_envelope_bytes,
            max_blocking_tasks_per_actor: self.max_blocking_tasks_per_actor,
            blocking_shutdown_timeout: self.blocking_shutdown_timeout,
            ingresses,
            running: AtomicBool::new(false),
            observability: GraphObservability::new(graph_name),
        }))
    }
}
