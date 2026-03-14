//! Static actor graphs for Tokio, intended to compose with `tokio-supervisor`.
//!
//! `tokio-actor` focuses on two responsibilities only:
//!
//! - defining a graph of actors and directed links between them
//! - moving byte envelopes between actors and external ingress points
//!
//! Internal restart policy and supervision are intentionally out of scope.
//! When an actor fails, panics, or exits before shutdown, the whole graph run
//! fails. That makes a graph instance a good fit for hosting as a single child
//! in a `tokio-supervisor` tree.
//!
//! # Core concepts
//!
//! | Type | Role |
//! |------|------|
//! | [`GraphBuilder`] | Constructs and validates the actor graph. |
//! | [`Graph`] | Immutable, cloneable graph spec that can be rerun. |
//! | [`ActorSet`] | Decomposed graph where actors can be run independently. |
//! | [`RunnableActor`] | One actor plus stable peer wiring for per-actor supervision. |
//! | [`ActorSpec`] | Actor definition. |
//! | [`ActorContext`] | Mailbox, peers, and shutdown token visible to one actor. |
//! | [`ActorRef`] | Cloneable stable mailbox sender for a linked peer. |
//! | [`IngressHandle`] | Stable external sender that survives graph reruns. |
//! | [`Envelope`] | Opaque byte payload passed through the graph. |
//!
//! # Stable mailbox handles
//!
//! `ActorRef` and `IngressHandle` are bound to long-lived mailbox bindings
//! instead of a single actor runtime. When a graph is rerun or a decomposed
//! actor is restarted from the same graph wiring, those handles transparently
//! follow the current mailbox for the target actor.
//!
//! This is especially useful when the graph is hosted inside a supervised
//! child task and can be restarted by `tokio-supervisor`, or when a graph is
//! decomposed with [`Graph::into_actor_set`] for per-actor supervision.
//!
//! # Observability
//!
//! `tokio-actor` follows the same backend-agnostic pattern as
//! `tokio-supervisor`:
//!
//! - `tracing` spans and structured logs are emitted automatically for graph,
//!   actor, ingress, and blocking-task lifecycle.
//! - optional `metrics` counters, gauges, and histograms are available via the
//!   `metrics` cargo feature.
//!
//! Install subscribers and metric recorders in the application boundary or an
//! example binary, not inside the library.
//!
//! # Resource limits
//!
//! Graphs apply conservative defaults for externally-controlled work:
//!
//! - mailboxes still default to 64 queued messages per actor
//! - envelopes larger than 1 MiB are rejected before entering a mailbox
//! - actors may run at most 16 blocking tasks concurrently by default
//! - shutdown waits up to 5 seconds for blocking tasks to stop, then detaches
//!   any remaining work so the graph can terminate
//!
//! Use [`GraphBuilder`] to tune or disable these limits for a specific graph.
//! Blocking closures should call [`BlockingContext::checkpoint`] or otherwise
//! observe cancellation regularly when graceful shutdown matters.
//!
//! # Quick start
//!
//! ```no_run
//! use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let graph = GraphBuilder::new()
//!     .name("example")
//!     .actor(ActorSpec::from_actor("frontend", |mut ctx: ActorContext| async move {
//!         while let Some(envelope) = ctx.recv().await {
//!             ctx.send("worker", envelope).await?;
//!         }
//!         Ok(())
//!     }))
//!     .actor(ActorSpec::from_actor("worker", |mut ctx: ActorContext| async move {
//!         while let Some(envelope) = ctx.recv().await {
//!             println!("{:?}", envelope.as_slice());
//!         }
//!         Ok(())
//!     }))
//!     .link("frontend", "worker")
//!     .ingress("requests", "frontend")
//!     .build()
//!     .expect("valid graph");
//!
//! let mut ingress = graph.ingress("requests").expect("ingress exists");
//! let stop = tokio_util::sync::CancellationToken::new();
//! let task = tokio::spawn({
//!     let graph = graph.clone();
//!     let stop = stop.clone();
//!     async move { graph.run_until(stop.cancelled()).await }
//! });
//!
//! ingress.wait_for_binding().await;
//! ingress.send(Envelope::from_static(b"hello")).await.expect("send succeeded");
//!
//! stop.cancel();
//! task.await.expect("graph task joined").expect("graph stopped cleanly");
//! # Ok(())
//! # }
//! ```
//!
//! For larger actor implementations, you can define a reusable actor type and
//! pass it to [`ActorSpec::from_actor`]:
//!
//! ```no_run
//! use tokio_actor::{Actor, ActorContext, ActorResult, ActorSpec, GraphBuilder};
//!
//! #[derive(Clone)]
//! struct Worker;
//!
//! impl Actor for Worker {
//!     async fn run(&self, mut ctx: ActorContext) -> ActorResult {
//!         while let Some(envelope) = ctx.recv().await {
//!             println!("{:?}", envelope.as_slice());
//!         }
//!         Ok(())
//!     }
//! }
//!
//! let graph = GraphBuilder::new()
//!     .name("example")
//!     .actor(ActorSpec::from_actor("worker", Worker))
//!     .build()
//!     .expect("valid graph");
//! # let _ = graph;
//! ```
//!
//! # Cargo features
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `metrics` | no | Enables `metrics` crate integration for counters, gauges, and histograms. |
//!
//! # Examples
//!
//! - `examples/ingress_rebind.rs` — stable ingress handles across graph reruns.
//! - `examples/send_vs_send_when_ready.rs` — retry-across-restart semantics.
//! - `examples/mailbox_backpressure.rs` — bounded mailbox back-pressure.
//! - `examples/envelope_limits.rs` — per-graph envelope size limits.
//! - `examples/blocking_work.rs` — spawning tracked blocking tasks.
//! - `examples/blocking_lifecycle.rs` — blocking task lifecycle handling.
//! - `examples/blocking_limits.rs` — per-actor blocking task concurrency.
//! - `examples/graph_failures.rs` — error propagation from actor failures.
//! - `examples/builder_validation.rs` — build-time graph validation errors.
//! - `examples/tracing.rs` — structured logging output.
//! - `examples/metrics.rs` — metrics integration (requires `--features metrics`).

mod actor;
mod actor_set;
mod binding;
mod blocking;
mod builder;
mod context;
mod envelope;
mod error;
mod graph;
mod ingress;
mod observability;
mod registry;

pub mod prelude {
    pub use crate::{
        Actor, ActorContext, ActorRef, ActorRegistry, ActorResult, ActorRunError, ActorSet,
        ActorSpec, BlockingContext, BlockingHandle, BlockingOperationError, BlockingOptions,
        BlockingTaskError, BlockingTaskFailure, BlockingTaskId, BuildError, Envelope, Graph,
        GraphBuilder, GraphError, IngressError, IngressHandle, RegistryError, RunnableActor,
        RunnableActorFactory, SendError, SpawnBlockingError,
    };
}

pub use actor::{Actor, ActorResult, ActorSpec, BoxError};
pub use actor_set::{ActorRunError, ActorSet, RunnableActor, RunnableActorFactory};
pub use blocking::{
    BlockingContext, BlockingHandle, BlockingOperationError, BlockingOptions, BlockingTaskError,
    BlockingTaskFailure, BlockingTaskId, SpawnBlockingError,
};
pub use builder::GraphBuilder;
pub use context::{ActorContext, ActorRef};
pub use envelope::Envelope;
pub use error::{BuildError, GraphError, IngressError, SendError};
pub use graph::Graph;
pub use ingress::IngressHandle;
pub use registry::{ActorRegistry, RegistryError};
