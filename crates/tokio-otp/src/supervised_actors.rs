use std::collections::HashMap;

use tokio_actor::{ActorSet, Graph, IngressHandle};
use tokio_supervisor::{
    ChildSpec, Restart, RestartIntensity, ShutdownPolicy, Supervisor, SupervisorBuilder,
};

use crate::{error::BuildError, runtime::Runtime};

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

    /// Builds reusable child specs plus stable ingress handles.
    pub fn build(self) -> Result<(Vec<ChildSpec>, HashMap<String, IngressHandle>), BuildError> {
        self.validate_overrides()?;

        let children = self
            .actor_set
            .actors()
            .iter()
            .cloned()
            .map(|actor| self.actor_child(actor))
            .collect();
        let ingresses = self.actor_set.ingresses();

        Ok((children, ingresses))
    }

    /// Adds the actor children to a supervisor builder and returns the built
    /// supervisor plus stable ingress handles.
    pub fn build_supervisor(
        self,
        builder: SupervisorBuilder,
    ) -> Result<(Supervisor, HashMap<String, IngressHandle>), BuildError> {
        let (children, ingresses) = self.build()?;
        let builder = children
            .into_iter()
            .fold(builder, |builder, child| builder.child(child));
        let supervisor = builder.build()?;
        Ok((supervisor, ingresses))
    }

    /// Adds the actor children to a supervisor builder and packages the result
    /// into a [`Runtime`].
    pub fn build_runtime(self, builder: SupervisorBuilder) -> Result<Runtime, BuildError> {
        let (supervisor, ingresses) = self.build_supervisor(builder)?;
        Ok(Runtime::new(supervisor, ingresses))
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

    fn actor_child(&self, actor: tokio_actor::RunnableActor) -> ChildSpec {
        let overrides = self.overrides.get(actor.id()).copied().unwrap_or_default();
        let actor_id = actor.id().to_owned();
        let mut child = ChildSpec::new(actor_id, move |ctx| {
            let actor = actor.clone();
            async move {
                actor
                    .run_until(ctx.token.cancelled())
                    .await
                    .map_err(Into::into)
            }
        })
        .restart(overrides.restart.unwrap_or(self.default_restart))
        .shutdown(overrides.shutdown.unwrap_or(self.default_shutdown));

        if let Some(intensity) = overrides.restart_intensity {
            child = child.restart_intensity(intensity);
        }

        child
    }
}
