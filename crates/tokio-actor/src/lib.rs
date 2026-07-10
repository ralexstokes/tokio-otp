//! Static typed actor graphs for Tokio, intended to compose with
//! `tokio-supervisor`.
//!
//! `tokio-actor` focuses on two responsibilities only:
//!
//! - defining a graph of actors with typed mailboxes
//! - providing restart-stable typed actor references
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
//! | [`RunnableActor`] | One actor plus stable binding for per-actor supervision. |
//! | [`RawActor`] | Custom-loop typed actor definition. |
//! | [`Actor`] | Handler-style actor definition with a provided receive loop. |
//! | [`ActorContext`] | Mailbox, registry, blocking work, and shutdown token visible to one actor. |
//! | [`ActorRef`] | Cloneable stable typed mailbox sender. |
//! | [`Reply`] | One-shot response channel carried inside request messages. |
//!
//! # Stable mailbox handles
//!
//! `ActorRef<M>` is bound to a long-lived mailbox binding instead of a single
//! actor runtime. When a graph is rerun or a decomposed actor is restarted
//! from the same graph wiring, those handles transparently follow the current
//! mailbox for the target actor.
//!
//! This is especially useful when the graph is hosted inside a supervised
//! child task and can be restarted by `tokio-supervisor`, or when a graph is
//! decomposed with [`Graph::into_actor_set`] for per-actor supervision.
//!
//! # Message loss at shutdown and restart
//!
//! [`ActorContext::recv`] is fail-fast during shutdown: it returns `None` as
//! soon as shutdown is requested, even when messages are still queued. A
//! hand-written actor loop that exits at that point drops those queued
//! messages, and queued [`ActorRef::call`] requests observe
//! [`CallError::ReplyDropped`].
//!
//! Actors that must finish queued work before stopping have two drain options:
//! implement [`Actor`] and return [`DrainPolicy::Drain`], or write a
//! custom [`RawActor::run`] loop that calls [`ActorContext::try_recv`] after
//! shutdown. Drains are not separately time-bounded; graph-run actors are
//! still limited by [`GraphBuilder::actor_shutdown_timeout`], and runnable
//! actors hosted under `tokio-supervisor` are limited by the child shutdown
//! policy.
//!
//! Restarts also lose queued messages. Each actor run binds a fresh mailbox,
//! so messages queued behind a poison message are dropped with the old
//! mailbox. [`ActorRef::send`] waits across restart windows when supervision
//! policy says a rebind is expected, but it does not resurrect messages that
//! were already accepted by the old mailbox.
//!
//! # Observability
//!
//! `tokio-actor` follows the same backend-agnostic pattern as
//! `tokio-supervisor`:
//!
//! - `tracing` spans and structured logs are emitted automatically for graph,
//!   actor, and mailbox lifecycle.
//! - optional `metrics` counters, gauges, and histograms are available via the
//!   `metrics` cargo feature.
//!
//! Install subscribers and metric recorders in the application boundary or an
//! example binary, not inside the library.
//!
//! # Resource limits
//!
//! Graph mailboxes default to 64 queued messages per actor. Use
//! [`GraphBuilder`] to tune that bound for a specific graph. Blocking work run
//! through [`ActorContext::run_blocking`] is cooperatively cancelled and uses
//! the actor shutdown timeout as its backstop.
//!
//! # Quick start
//!
//! ```no_run
//! use tokio_actor::{ActorContext, ActorResult, GraphBuilder, Actor, Reply};
//!
//! enum CounterMsg {
//!     Add(u64),
//!     Total(Reply<u64>),
//! }
//!
//! #[derive(Clone)]
//! struct Counter {
//!     total: u64,
//! }
//!
//! impl Actor for Counter {
//!     type Msg = CounterMsg;
//!
//!     async fn handle(
//!         &mut self,
//!         message: CounterMsg,
//!         _ctx: &ActorContext<CounterMsg>,
//!     ) -> ActorResult {
//!         match message {
//!             CounterMsg::Add(n) => self.total += n,
//!             CounterMsg::Total(reply) => reply.send(self.total),
//!         }
//!         Ok(())
//!     }
//! }
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut builder = GraphBuilder::new();
//! builder.name("example");
//! let counter = builder.add(Counter { total: 0 });
//! let graph = builder.build().expect("valid graph");
//!
//! let handle = graph.spawn()?;
//!
//! counter.send(CounterMsg::Add(2)).await.expect("send succeeded");
//! counter.send(CounterMsg::Add(3)).await.expect("send succeeded");
//! assert_eq!(counter.call(CounterMsg::Total).await?, 5);
//!
//! handle.shutdown_and_wait().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Static topologies
//!
//! For cyclic actor graphs, derive [`Topology`] on a named-field struct whose
//! fields are the actors. The wiring closure receives typed refs for every
//! field before any actor is constructed.
//!
//! ```no_run
//! use tokio_actor::{ActorContext, ActorRef, ActorResult, Actor, Topology};
//!
//! enum FrontendMsg {
//!     Feed(String),
//!     Ack,
//! }
//!
//! struct ParserMsg(String);
//! struct SinkMsg(String);
//!
//! #[derive(Clone)]
//! struct Frontend {
//!     parser: ActorRef<ParserMsg>,
//! }
//!
//! impl Actor for Frontend {
//!     type Msg = FrontendMsg;
//!
//!     async fn handle(
//!         &mut self,
//!         message: FrontendMsg,
//!         _ctx: &ActorContext<FrontendMsg>,
//!     ) -> ActorResult {
//!         match message {
//!             FrontendMsg::Feed(line) => self.parser.send(ParserMsg(line)).await?,
//!             FrontendMsg::Ack => {}
//!         }
//!         Ok(())
//!     }
//! }
//!
//! #[derive(Clone)]
//! struct Parser {
//!     frontend: ActorRef<FrontendMsg>,
//!     sink: ActorRef<SinkMsg>,
//! }
//!
//! impl Actor for Parser {
//!     type Msg = ParserMsg;
//!
//!     async fn handle(
//!         &mut self,
//!         message: ParserMsg,
//!         _ctx: &ActorContext<ParserMsg>,
//!     ) -> ActorResult {
//!         self.sink.send(SinkMsg(message.0)).await?;
//!         self.frontend.send(FrontendMsg::Ack).await?;
//!         Ok(())
//!     }
//! }
//!
//! #[derive(Clone)]
//! struct Sink;
//!
//! impl Actor for Sink {
//!     type Msg = SinkMsg;
//!
//!     async fn handle(&mut self, _message: SinkMsg, _ctx: &ActorContext<SinkMsg>) -> ActorResult {
//!         Ok(())
//!     }
//! }
//!
//! #[derive(Topology)]
//! struct Pipeline {
//!     frontend: Frontend,
//!     parser: Parser,
//!     sink: Sink,
//! }
//!
//! # fn build() -> Result<(), Box<dyn std::error::Error>> {
//! let graph = Pipeline::graph(|refs| Pipeline {
//!     frontend: Frontend {
//!         parser: refs.parser.clone(),
//!     },
//!     parser: Parser {
//!         frontend: refs.frontend.clone(),
//!         sink: refs.sink.clone(),
//!     },
//!     sink: Sink,
//! })?;
//! # let _ = graph;
//! # Ok(())
//! # }
//! ```
//!
//! # Cargo features
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `derive` | yes | Re-exports `#[derive(Topology)]`. |
//! | `metrics` | no | Enables `metrics` crate integration for counters, gauges, and histograms. |

mod actor;
mod actor_set;
mod binding;
mod builder;
mod context;
mod error;
mod graph;
mod handler;
mod observability;
mod registry;

pub mod prelude {
    // Keep this list mirrored by tokio_otp::prelude; its prelude test guards drift.
    #[cfg(feature = "derive")]
    pub use crate::Topology;
    pub use crate::{
        Actor, ActorContext, ActorRef, ActorRegistry, ActorResult, ActorRunError, ActorSet,
        ActorSlot, BoxError, CallError, DrainPolicy, Graph, GraphBuildError, GraphBuilder,
        GraphError, GraphHandle, LookupError, RawActor, RebindPolicy, RegistryError, Reply,
        RunnableActor, RunnableActorFactory, SendError, TryRecvError,
    };
}

pub use actor::{ActorResult, BoxError, RawActor};
pub use actor_set::{ActorRunError, ActorSet, RunnableActor, RunnableActorFactory};
pub use binding::RebindPolicy;
pub use builder::{ActorSlot, GraphBuilder};
pub use context::{ActorContext, ActorRef, Reply};
pub use error::{CallError, GraphBuildError, GraphError, LookupError, SendError};
pub use graph::{Graph, GraphHandle};
pub use handler::{Actor, DrainPolicy};
pub use registry::{ActorRegistry, RegistryError};
pub use tokio::sync::mpsc::error::TryRecvError;
#[cfg(feature = "derive")]
pub use tokio_actor_derive::Topology;
