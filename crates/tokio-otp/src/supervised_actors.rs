use std::collections::HashMap;

use crate::{ActorRef, Graph};
use tokio_supervisor::{
    ChildSpec, Restart, RestartIntensity, ShutdownPolicy, Supervisor, SupervisorBuilder,
};

use crate::runtime::{Runtime, actor_child_spec};

#[derive(Clone, Copy, Debug, Default)]
struct ActorOverrides {
    restart: Option<Restart>,
    restart_intensity: Option<RestartIntensity>,
    shutdown: Option<ShutdownPolicy>,
}

/// Builder that adapts each actor in a graph into its own supervised child.
#[derive(Clone, Debug)]
pub struct SupervisedActors {
    graph: Graph,
    default_restart: Restart,
    default_shutdown: ShutdownPolicy,
    overrides: HashMap<String, ActorOverrides>,
}

impl SupervisedActors {
    /// Adapts a graph into per-actor supervised children.
    pub fn new(graph: Graph) -> Self {
        Self {
            graph,
            default_restart: Restart::Transient,
            default_shutdown: ShutdownPolicy::default(),
            overrides: HashMap::new(),
        }
    }

    /// Sets the default restart policy applied to every actor child.
    #[must_use]
    pub fn restart(mut self, restart: Restart) -> Self {
        self.default_restart = restart;
        self
    }

    /// Sets the default shutdown policy applied to every actor child.
    #[must_use]
    pub fn shutdown(mut self, shutdown: ShutdownPolicy) -> Self {
        self.default_shutdown = shutdown;
        self
    }

    /// Overrides the restart policy for the actor identified by this typed ref.
    #[must_use]
    pub fn actor_restart<M>(mut self, actor: &ActorRef<M>, restart: Restart) -> Self {
        self.overrides
            .entry(actor.id().to_owned())
            .or_default()
            .restart = Some(restart);
        self
    }

    /// Overrides restart intensity for the actor identified by this typed ref.
    #[must_use]
    pub fn actor_restart_intensity<M>(
        mut self,
        actor: &ActorRef<M>,
        intensity: RestartIntensity,
    ) -> Self {
        self.overrides
            .entry(actor.id().to_owned())
            .or_default()
            .restart_intensity = Some(intensity);
        self
    }

    /// Overrides shutdown policy for the actor identified by this typed ref.
    #[must_use]
    pub fn actor_shutdown<M>(mut self, actor: &ActorRef<M>, shutdown: ShutdownPolicy) -> Self {
        self.overrides
            .entry(actor.id().to_owned())
            .or_default()
            .shutdown = Some(shutdown);
        self
    }

    /// Builds reusable child specs.
    pub fn build(self) -> Vec<ChildSpec> {
        self.actor_children()
    }

    /// Adds the actor children to a supervisor builder and returns the built supervisor.
    pub fn build_supervisor(
        self,
        builder: SupervisorBuilder,
    ) -> Result<Supervisor, tokio_supervisor::SupervisorBuildError> {
        let children = self.build();
        let builder = children
            .into_iter()
            .fold(builder, |builder, child| builder.child(child));
        let supervisor = builder.build()?;
        Ok(supervisor)
    }

    /// Adds the actor children to a supervisor builder and packages the result
    /// into a [`Runtime`].
    pub fn build_runtime(
        self,
        builder: SupervisorBuilder,
    ) -> Result<Runtime, tokio_supervisor::SupervisorBuildError> {
        let actor_factory = self.graph.dynamic_factory();
        let actors = self.graph.actors().to_vec();
        let children = self.actor_children();
        let builder = children
            .into_iter()
            .fold(builder, |builder, child| builder.child(child));
        let supervisor = builder.build()?;

        Ok(Runtime::with_actors(supervisor, actor_factory, actors))
    }

    fn actor_children(&self) -> Vec<ChildSpec> {
        self.graph
            .actors()
            .iter()
            .cloned()
            .map(|actor| self.actor_child(actor))
            .collect()
    }

    fn actor_child(&self, actor: crate::RunnableActor) -> ChildSpec {
        let overrides = self
            .overrides
            .get(actor.label())
            .copied()
            .unwrap_or_default();
        actor_child_spec(
            actor,
            overrides.restart.unwrap_or(self.default_restart),
            overrides.shutdown.unwrap_or(self.default_shutdown),
            overrides.restart_intensity,
        )
    }
}
