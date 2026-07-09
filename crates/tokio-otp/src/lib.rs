//! The front door to OTP-style fault tolerance on `tokio`: supervised typed
//! actor graphs with one integrated [`Runtime`].
//!
//! For the common setup — every actor of a graph running as its own
//! supervised child — you need one import and one builder:
//!
//! ```no_run
//! use tokio_otp::prelude::*;
//!
//! #[derive(Clone)]
//! struct Echo;
//!
//! impl MessageHandler for Echo {
//!     type Msg = String;
//!
//!     async fn handle(&mut self, message: String, _ctx: &ActorContext<String>) -> ActorResult {
//!         println!("{message}");
//!         Ok(())
//!     }
//! }
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut graph = GraphBuilder::new();
//! let echo = graph.actor("echo", Echo);
//!
//! let runtime = Runtime::builder()
//!     .graph(graph.build()?)
//!     .strategy(Strategy::OneForOne)
//!     .build()?;
//! let handle = runtime.spawn();
//!
//! echo.send("hello".to_owned()).await?;
//! handle.shutdown_and_wait().await?;
//! # Ok(())
//! # }
//! ```
//!
//! See [`RuntimeBuilder`] for a complete example. The [`prelude`] re-exports
//! the common types of the underlying `tokio-actor` and `tokio-supervisor`
//! crates, which remain independently usable à la carte.
//!
//! # Core types
//!
//! | Type | Role |
//! |------|------|
//! | [`Runtime`] / [`RuntimeBuilder`] | Owns a supervisor and optional dynamic actor registry — the common composition. |
//! | [`RuntimeHandle`] | Control surface for a spawned runtime (shutdown, dynamic actors, observability). |
//! | [`SupervisedActors`] | Decomposes a graph into per-actor supervised children with configurable policies. |
//! | [`SupervisedGraph`] | Wraps a whole graph as a single supervised child. |
//! | [`DynamicActorOptions`] | Options for runtime-added actors (restart, shutdown). |
//!
//! # Composition modes
//!
//! - **The integrated runtime** via [`Runtime::builder`]: per-actor
//!   supervision with uniform policies, packaged with a dynamic actor
//!   registry. Add [`RuntimeBuilder::dynamic`] to start with no actors or to
//!   let a graph-backed runtime return to zero actors.
//!
//! - **Per-actor supervision** via [`SupervisedActors`]: each actor becomes its
//!   own child in a supervisor, with individual restart and shutdown policies.
//!   Use [`build_runtime`](SupervisedActors::build_runtime) for the integrated
//!   [`Runtime`] or [`build`](SupervisedActors::build) for raw child specs.
//!
//! - **Whole-graph supervision** via [`SupervisedGraph`]: the entire actor graph
//!   runs as a single child. Simpler, but all actors restart together.
//!
//! # Examples
//!
//! - `examples/supervised_actors.rs` — per-actor supervision with default
//!   policies.
//! - `examples/drain_policy.rs` — draining queued actor messages during
//!   shutdown.
//! - `examples/individual_actor_policies.rs` — per-actor restart/shutdown
//!   overrides.
//! - `examples/dynamic_actors.rs` — adding and removing actors at runtime.
//! - `examples/supervisor_snapshot_trace.rs` — observing runtime state.
//! - `examples/console.rs` — serving the web console for a supervised runtime.

mod builder;
mod error;
mod runtime;
mod supervised_actors;
mod supervised_graph;

/// Common imports for `tokio-otp` consumers.
///
/// Re-exports the documented union of the `tokio-actor` and
/// `tokio-supervisor` preludes, except the conflicts documented below, plus
/// this crate's runtime types.
pub mod prelude {
    // Exclusions: tokio_supervisor::BoxError is the same type as the
    // tokio_actor alias exported here, so omitting the duplicate is permanent
    // and harmless; console types are feature-gated and experimental, so they
    // remain available only at the crate root.
    pub use tokio_actor::{
        Actor, ActorContext, ActorRef, ActorRegistry, ActorResult, ActorRunError, ActorSet,
        BlockingContext, BlockingHandle, BlockingOperationError, BlockingOptions,
        BlockingTaskError, BlockingTaskFailure, BlockingTaskId, BoxError, CallError, DrainPolicy,
        Graph, GraphBuildError, GraphBuilder, GraphError, GraphHandle, LookupError, MessageHandler,
        RebindPolicy, RegistryError, Reply, RunnableActor, RunnableActorFactory, SendError,
        SpawnBlockingError, TryRecvError,
    };
    pub use tokio_supervisor::{
        BackoffPolicy, ChildContext, ChildMembershipView, ChildResult, ChildSnapshot, ChildSpec,
        ChildStateView, ControlError, EventPathSegment, ExitStatusView, Restart, RestartIntensity,
        RestartMonitor, RestartMonitorError, ShutdownMode, ShutdownPolicy, Strategy, Supervisor,
        SupervisorBuildError, SupervisorBuilder, SupervisorError, SupervisorEvent, SupervisorExit,
        SupervisorHandle, SupervisorSnapshot, SupervisorStateView, SupervisorToken,
        prelude::{SupervisorEventReceiverExt, SupervisorSnapshotReceiverExt},
    };

    pub use crate::{
        DynamicActorError, DynamicActorOptions, Runtime, RuntimeBuildError, RuntimeBuilder,
        RuntimeHandle, SupervisedActors, SupervisedGraph,
    };
}

#[cfg(feature = "console")]
pub use tokio_otp_console::{Console, ConsoleBuilder, ConsoleHandle};

pub use builder::RuntimeBuilder;
pub use error::{DynamicActorError, RuntimeBuildError};
pub use runtime::{DynamicActorOptions, Runtime, RuntimeHandle};
pub use supervised_actors::SupervisedActors;
pub use supervised_graph::SupervisedGraph;
pub use tokio_actor::{DrainPolicy, MessageHandler};
