use std::collections::HashMap;

use tokio_actor::{ActorRegistry, ActorSet, Graph};
use tokio_supervisor::{
    ChildSpec, Restart, RestartIntensity, ShutdownPolicy, Supervisor, SupervisorBuilder,
};

use crate::{
    error::BuildError,
    runtime::{Runtime, actor_child_spec},
};

#[derive(Clone, Copy, Debug, Default)]
struct ActorOverrides {
    restart: Option<Restart>,
    restart_intensity: Option<RestartIntensity>,
    shutdown: Option<ShutdownPolicy>,
}

/// Builder that adapts each actor in a graph into its own supervised child.
#[derive(Clone, Debug)]
pub struct SupervisedActors {
    actor_set: ActorSet,
    default_restart: Restart,
    default_shutdown: ShutdownPolicy,
    overrides: HashMap<String, ActorOverrides>,
}

impl SupervisedActors {
    /// Decomposes a graph into per-actor supervised children.
    pub fn new(graph: Graph) -> Result<Self, BuildError> {
        let actor_set = graph.into_actor_set()?;

        Ok(Self {
            actor_set,
            default_restart: Restart::Transient,
            default_shutdown: ShutdownPolicy::default(),
            overrides: HashMap::new(),
        })
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

    /// Overrides the restart policy for one actor.
    #[must_use]
    pub fn actor_restart(mut self, actor_id: impl Into<String>, restart: Restart) -> Self {
        self.overrides.entry(actor_id.into()).or_default().restart = Some(restart);
        self
    }

    /// Overrides the restart intensity for one actor.
    #[must_use]
    pub fn actor_restart_intensity(
        mut self,
        actor_id: impl Into<String>,
        intensity: RestartIntensity,
    ) -> Self {
        self.overrides
            .entry(actor_id.into())
            .or_default()
            .restart_intensity = Some(intensity);
        self
    }

    /// Overrides the shutdown policy for one actor.
    #[must_use]
    pub fn actor_shutdown(mut self, actor_id: impl Into<String>, shutdown: ShutdownPolicy) -> Self {
        self.overrides.entry(actor_id.into()).or_default().shutdown = Some(shutdown);
        self
    }

    /// Builds reusable child specs.
    pub fn build(self) -> Result<Vec<ChildSpec>, BuildError> {
        self.validate_overrides()?;
        Ok(self.actor_children())
    }

    /// Adds the actor children to a supervisor builder and returns the built supervisor.
    pub fn build_supervisor(self, builder: SupervisorBuilder) -> Result<Supervisor, BuildError> {
        let children = self.build()?;
        let builder = children
            .into_iter()
            .fold(builder, |builder, child| builder.child(child));
        let supervisor = builder.build()?;
        Ok(supervisor)
    }

    /// Adds the actor children to a supervisor builder and packages the result
    /// into a [`Runtime`].
    pub fn build_runtime(self, builder: SupervisorBuilder) -> Result<Runtime, BuildError> {
        self.validate_overrides()?;

        let registry = ActorRegistry::new();
        for actor in self.actor_set.actors() {
            actor.set_registry(registry.clone());
            actor.register_with(&registry)?;
        }

        let actor_factory = self.actor_set.dynamic_factory();
        let children = self.actor_children();
        let builder = children
            .into_iter()
            .fold(builder, |builder, child| builder.child(child));
        let supervisor = builder.build()?;

        Ok(Runtime::with_dynamic(supervisor, registry, actor_factory))
    }

    fn validate_overrides(&self) -> Result<(), BuildError> {
        for actor_id in self.overrides.keys() {
            if self.actor_set.actor(actor_id).is_none() {
                return Err(BuildError::UnknownActor {
                    actor_id: actor_id.clone(),
                });
            }
        }

        Ok(())
    }

    fn actor_children(&self) -> Vec<ChildSpec> {
        self.actor_set
            .actors()
            .iter()
            .cloned()
            .map(|actor| self.actor_child(actor))
            .collect()
    }

    fn actor_child(&self, actor: tokio_actor::RunnableActor) -> ChildSpec {
        let overrides = self.overrides.get(actor.id()).copied().unwrap_or_default();
        actor_child_spec(
            actor,
            overrides.restart.unwrap_or(self.default_restart),
            overrides.shutdown.unwrap_or(self.default_shutdown),
            overrides.restart_intensity,
        )
    }
}
