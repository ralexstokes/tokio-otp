//! The front door to OTP-style fault tolerance on `tokio`: supervised typed
//! actor graphs with one integrated [`Runtime`].
//!
//! For the common setup — every actor of a graph running as its own
//! supervised child — you need one import and one builder:
//!
//! ```ignore
//! use tokio_otp::prelude::*;
//!
//! let runtime = Runtime::builder()
//!     .graph(graph)
//!     .strategy(Strategy::OneForOne)
//!     .build()?;
//! let handle = runtime.spawn();
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
//!   registry. The right default for most applications.
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
/// Re-exports key types from `tokio-actor`, `tokio-supervisor`, and this
/// crate so a single `use tokio_otp::prelude::*;` brings everything needed
/// for typical supervised-actor setups.
pub mod prelude {
    pub use tokio_actor::{
        Actor, ActorContext, ActorRef, ActorRegistry, ActorResult, ActorRunError, ActorSet,
        BoxError, Graph, GraphBuilder, LookupError, Reply, RunnableActor, RunnableActorFactory,
    };
    pub use tokio_supervisor::{
        ChildContext, ChildSpec, Restart, RestartIntensity, ShutdownMode, ShutdownPolicy, Strategy,
        Supervisor, SupervisorBuilder, SupervisorEvent, SupervisorHandle, SupervisorSnapshot,
    };

    pub use crate::{
        DynamicActorError, DynamicActorOptions, Runtime, RuntimeBuilder, RuntimeHandle,
        SupervisedActors, SupervisedGraph,
    };
}

#[cfg(feature = "console")]
pub use tokio_otp_console::{Console, ConsoleBuilder, ConsoleHandle};

pub use builder::RuntimeBuilder;
pub use error::{BuildError, DynamicActorError};
pub use runtime::{DynamicActorOptions, Runtime, RuntimeHandle};
pub use supervised_actors::SupervisedActors;
pub use supervised_graph::SupervisedGraph;
