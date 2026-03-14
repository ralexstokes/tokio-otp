//! OTP-style composition helpers for `tokio-actor` and `tokio-supervisor`.
//!
//! `tokio-otp` keeps the actor and supervisor crates independent while
//! removing the boilerplate of composing them together.
//!
//! # Core types
//!
//! | Type | Role |
//! |------|------|
//! | [`SupervisedActors`] | Decomposes a graph into per-actor supervised children with configurable policies. |
//! | [`SupervisedGraph`] | Wraps a whole graph as a single supervised child. |
//! | [`Runtime`] | Owns a supervisor and stable ingress handles — the common composition. |
//! | [`RuntimeHandle`] | Control surface for a spawned runtime (shutdown, dynamic actors, observability). |
//! | [`DynamicActorOptions`] | Options for runtime-added actors (restart, shutdown, peers). |
//!
//! # Two composition modes
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
        Actor, ActorContext, ActorRef, ActorRegistry, ActorRunError, ActorSet, ActorSpec, Envelope,
        Graph, GraphBuilder, IngressHandle, RunnableActor, RunnableActorFactory,
    };
    pub use tokio_supervisor::{
        ChildContext, ChildSpec, Restart, RestartIntensity, ShutdownMode, ShutdownPolicy, Strategy,
        Supervisor, SupervisorBuilder, SupervisorEvent, SupervisorHandle, SupervisorSnapshot,
    };

    pub use crate::{DynamicActorError, DynamicActorOptions, SupervisedActors, SupervisedGraph};
}

#[cfg(feature = "console")]
pub use tokio_otp_console::{Console, ConsoleBuilder, ConsoleHandle};

pub use error::{BuildError, DynamicActorError};
pub use runtime::{DynamicActorOptions, Runtime, RuntimeHandle};
pub use supervised_actors::SupervisedActors;
pub use supervised_graph::SupervisedGraph;
