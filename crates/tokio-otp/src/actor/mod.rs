//! Actor layer: typed actor graphs with restart-stable refs (formerly the
//! `tokio-actor` crate).
//!
//! This module tree is private; its public API is re-exported flat from the
//! crate root. The seam between the actor layer and the runtime layer —
//! the `RestartPolicy` → `RebindPolicy` mapping and the terminate-binding drop
//! guard — lives in `crate::runtime` and is a crate-internal invariant.

mod binding;
mod builder;
mod context;
mod error;
mod graph;
mod handler;
mod observability;
mod raw;

pub use binding::{ActorStats, RebindPolicy};
pub use builder::{ActorSlot, GraphBuilder};
pub use context::{ActorContext, ActorRef, Reply, TimerRef};
pub use error::{CallError, GraphBuildError, SendError};
pub use graph::{ActorRunError, Graph, RunnableActor, RunnableActorFactory};
pub use handler::{Actor, DrainPolicy};
pub use raw::{ActorResult, BoxError, RawActor};
