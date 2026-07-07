//! **Prototype**: typed messages and typed actor refs for `tokio-actor`.
//!
//! This crate validates the load-bearing design risk behind TODO items 1–3:
//! can the restart-stable mailbox binding in `tokio-actor` carry a
//! per-actor message type `M`, with type erasure confined to a single
//! registry map, without contaminating the whole crate with generics?
//!
//! The answer this prototype demonstrates:
//!
//! - [`MailboxRef<M>`](binding) and the binding watch-channel machinery
//!   generalize over `M` verbatim — the rebinding logic is unchanged from
//!   `tokio-actor/src/binding.rs`.
//! - The graph stores each actor's binding type-erased **once**
//!   (`Arc<dyn Any + Send + Sync>` around the typed binding core), and
//!   downcasts only when a typed [`ActorRef<M>`] is minted. Every subsequent
//!   send is statically typed with no per-send downcast.
//! - Because refs are restart-stable, [`GraphBuilder`] can mint an
//!   [`ActorRef<M>`] *before* the target actor is registered
//!   ([`GraphBuilder::declare`]), so typed refs have no declaration-order or
//!   cycle problem.
//! - Message-type mismatches and name typos are caught at
//!   [`GraphBuilder::build`], the same validation point the byte-envelope
//!   crate uses today.
//! - Request/response falls out for free: [`Reply<T>`] rides inside an
//!   actor's message enum and [`ActorRef::call`] correlates it with a
//!   oneshot.
//!
//! # Example
//!
//! ```no_run
//! use tokio_actor_typed::{ActorContext, GraphBuilder, Reply};
//!
//! enum CounterMsg {
//!     Add(u64),
//!     Total(Reply<u64>),
//! }
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut builder = GraphBuilder::new();
//! let mut counter = builder.actor_fn("counter", |mut ctx: ActorContext<CounterMsg>| async move {
//!     let mut total = 0;
//!     while let Some(message) = ctx.recv().await {
//!         match message {
//!             CounterMsg::Add(n) => total += n,
//!             CounterMsg::Total(reply) => reply.send(total),
//!         }
//!     }
//!     Ok(())
//! });
//! let graph = builder.build()?;
//!
//! let stop = tokio_util::sync::CancellationToken::new();
//! let run = tokio::spawn({
//!     let graph = graph.clone();
//!     let stop = stop.clone();
//!     async move { graph.run_until(stop.cancelled()).await }
//! });
//!
//! counter.wait_for_binding().await;
//! counter.send(CounterMsg::Add(2)).await?;
//! counter.send(CounterMsg::Add(3)).await?;
//! assert_eq!(counter.call(CounterMsg::Total).await?, 5);
//!
//! stop.cancel();
//! run.await??;
//! # Ok(())
//! # }
//! ```
//!
//! # Intentionally out of scope
//!
//! Everything orthogonal to the erasure question is omitted: observability,
//! blocking-task tracking, ingress codecs, `ActorSet` decomposition and the
//! `tokio-otp` supervision glue, link/edge topology metadata, and envelope
//! size limits (byte limits only make sense at byte boundaries; typed
//! mailboxes are bounded by depth). A graph must not be run concurrently
//! with itself; reruns after a previous run finishes are supported and
//! restart-stable refs follow the new mailboxes.
//!
//! # Open questions surfaced by the prototype
//!
//! - `Actor` uses an associated `Msg` type, which reads well but makes a
//!   blanket closure impl impossible (the message type parameter would be
//!   unconstrained, E0207). [`GraphBuilder::actor_fn`] papers over this with
//!   an adapter; a generic `Actor<M>` trait is the alternative trade-off.
//! - Edge/link metadata for the console and validation would be recorded via
//!   a wiring API (`wire.peer(&ref)`) in the real implementation.

mod actor;
mod binding;
mod builder;
mod context;
mod error;
mod graph;

pub use actor::{Actor, ActorResult};
pub use builder::GraphBuilder;
pub use context::{ActorContext, ActorRef, Reply};
pub use error::{BoxError, BuildError, CallError, GraphError, LookupError, SendError};
pub use graph::Graph;
