use tokio_actor::{ActorRef, Graph, LookupError};
use tokio_supervisor::ChildSpec;

/// Convenience wrapper for supervising a whole actor graph as a single child.
#[derive(Clone, Debug)]
pub struct SupervisedGraph {
    id: String,
    graph: Graph,
}

impl SupervisedGraph {
    /// Creates a new whole-graph child wrapper.
    pub fn new(id: impl Into<String>, graph: Graph) -> Self {
        Self {
            id: id.into(),
            graph,
        }
    }

    /// Returns a typed actor ref for a graph actor.
    pub fn actor_ref<M: Send + 'static>(&self, actor_id: &str) -> Result<ActorRef<M>, LookupError> {
        self.graph.actor_ref(actor_id)
    }

    /// Adapts the wrapped graph into a [`ChildSpec`].
    pub fn into_child_spec(self) -> ChildSpec {
        let id = self.id;
        let graph = self.graph;

        ChildSpec::new(id, move |ctx| {
            let graph = graph.clone();
            async move {
                graph
                    .run_until(ctx.shutdown_token().cancelled())
                    .await
                    .map_err(Into::into)
            }
        })
    }
}
