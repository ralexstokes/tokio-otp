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
//! | [`ActorSpec`] | Native Rust actor definition. |
//! | [`ActorContext`] | Mailbox, peers, and shutdown token visible to one actor. |
//! | [`ActorRef`] | Cloneable mailbox sender for a linked peer. |
//! | [`IngressHandle`] | Stable external sender that survives graph reruns. |
//! | [`Envelope`] | Opaque byte payload passed through the graph. |
//!
//! # Stable ingress handles
//!
//! Ingress handles are bound to a named entrypoint in the graph instead of a
//! single actor runtime. When the same [`Graph`] is rerun, the handle is
//! rebound to the current mailbox of its target actor.
//!
//! This is especially useful when the graph is hosted inside a supervised
//! child task and can be restarted by `tokio-supervisor`.
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

mod actor;
mod blocking;
mod builder;
mod context;
mod envelope;
mod error;
mod graph;
mod ingress;
mod observability;

pub mod prelude {
    pub use crate::{
        Actor, ActorContext, ActorRef, ActorResult, ActorSpec, BlockingContext, BlockingHandle,
        BlockingOperationError, BlockingOptions, BlockingTaskError, BlockingTaskFailure,
        BlockingTaskId, BuildError, Envelope, Graph, GraphBuilder, GraphError, IngressError,
        IngressHandle, SendError, SpawnBlockingError,
    };
}

pub use actor::{Actor, ActorResult, ActorSpec, BoxError};
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
