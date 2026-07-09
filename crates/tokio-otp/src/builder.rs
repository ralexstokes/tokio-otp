use tokio_actor::{ActorRegistry, Graph, RunnableActorFactory};
use tokio_supervisor::{Restart, RestartIntensity, ShutdownPolicy, Strategy, SupervisorBuilder};

use crate::{error::RuntimeBuildError, runtime::Runtime, supervised_actors::SupervisedActors};

/// One-stop builder for the common supervised-actor setup.
///
/// Wires an actor [`Graph`] into a [`Runtime`] where every actor runs as its
/// own supervised child, with dynamic actor support enabled. It can also build
/// a graph-less dynamic runtime via [`dynamic`](Self::dynamic). Created via
/// [`Runtime::builder`].
///
/// For per-actor policy overrides, or to compose actor children into a larger
/// supervision tree, drop down to [`SupervisedActors`] with a
/// [`SupervisorBuilder`].
///
/// # Example
///
/// ```no_run
/// use tokio_otp::prelude::*;
///
/// #[derive(Clone)]
/// struct Echo;
///
/// impl MessageHandler for Echo {
///     type Msg = String;
///
///     async fn handle(&mut self, message: String, _ctx: &ActorContext<String>) -> ActorResult {
///         println!("{message}");
///         Ok(())
///     }
/// }
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut graph = GraphBuilder::new();
/// let echo = graph.add(Echo);
///
/// let runtime = Runtime::builder()
///     .graph(graph.build()?)
///     .strategy(Strategy::OneForOne)
///     .restart(Restart::Transient)
///     .build()?;
/// let handle = runtime.spawn();
///
/// echo.send("hello".to_owned()).await?;
/// handle.shutdown_and_wait().await?;
/// # Ok(())
/// # }
/// ```
///
/// Dynamic-only runtimes can start without a graph:
///
/// ```no_run
/// use tokio_otp::prelude::*;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let runtime = Runtime::builder().dynamic().build()?;
/// let handle = runtime.spawn();
/// handle.shutdown_and_wait().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct RuntimeBuilder {
    graph: Option<Graph>,
    strategy: Strategy,
    restart: Restart,
    shutdown: ShutdownPolicy,
    restart_intensity: Option<RestartIntensity>,
    dynamic: bool,
}

impl RuntimeBuilder {
    /// Creates a builder with default settings: [`OneForOne`](Strategy::OneForOne)
    /// strategy, [`Transient`](Restart::Transient) restart, default shutdown
    /// policy, and no graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the actor graph to run. Required unless [`dynamic`](Self::dynamic)
    /// is enabled.
    #[must_use]
    pub fn graph(mut self, graph: Graph) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Allows the runtime to start with no actor graph and to idle with zero
    /// actors.
    ///
    /// A dynamic runtime runs until [`shutdown`](crate::RuntimeHandle::shutdown)
    /// is requested, even after every actor completes or is removed. If no
    /// graph is supplied, the builder's [`restart`](Self::restart) and
    /// [`shutdown`](Self::shutdown) settings have no static actors to apply to;
    /// runtime-added actors use their own [`DynamicActorOptions`](crate::DynamicActorOptions).
    #[must_use]
    pub fn dynamic(mut self) -> Self {
        self.dynamic = true;
        self
    }

    /// Sets the supervisor restart strategy. See [`Strategy`] for options.
    #[must_use]
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Sets the restart policy applied to every actor child.
    #[must_use]
    pub fn restart(mut self, restart: Restart) -> Self {
        self.restart = restart;
        self
    }

    /// Sets the shutdown policy applied to every actor child.
    #[must_use]
    pub fn shutdown(mut self, shutdown: ShutdownPolicy) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Sets the supervisor's default restart intensity.
    #[must_use]
    pub fn restart_intensity(mut self, intensity: RestartIntensity) -> Self {
        self.restart_intensity = Some(intensity);
        self
    }

    /// Validates the configuration and returns a ready-to-run [`Runtime`].
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeBuildError::MissingGraph`] if no graph was provided and
    /// [`dynamic`](Self::dynamic) was not enabled, or any error from
    /// decomposing the graph and building the supervisor.
    pub fn build(self) -> Result<Runtime, RuntimeBuildError> {
        let mut supervisor = SupervisorBuilder::new().strategy(self.strategy);
        if self.dynamic {
            supervisor = supervisor.allow_empty();
        }
        if let Some(intensity) = self.restart_intensity {
            supervisor = supervisor.restart_intensity(intensity);
        }

        match self.graph {
            Some(graph) => {
                let actors = SupervisedActors::new(graph)?
                    .restart(self.restart)
                    .shutdown(self.shutdown);
                actors.build_runtime(supervisor)
            }
            None if self.dynamic => {
                let supervisor = supervisor.build()?;
                Ok(Runtime::with_dynamic(
                    supervisor,
                    ActorRegistry::new(),
                    RunnableActorFactory::new(),
                ))
            }
            None => Err(RuntimeBuildError::MissingGraph),
        }
    }
}

impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeBuilder")
            .field("strategy", &self.strategy)
            .field("restart", &self.restart)
            .field("shutdown", &self.shutdown)
            .field("restart_intensity", &self.restart_intensity)
            .field("dynamic", &self.dynamic)
            .finish_non_exhaustive()
    }
}
