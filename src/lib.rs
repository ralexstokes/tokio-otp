//! Structured task supervision for Tokio, with observability hooks for:
//!
//! - [`SupervisorEvent`] subscriptions when you want to react programmatically.
//! - `tracing` spans and logs when you want structured lifecycle output.
//! - Optional `metrics` counters, gauges, and histograms for dashboards and alerts.
//!
//! Events are the most precise choice when your code needs to respond to supervision
//! changes directly. Tracing is better for operator-facing logs and distributed
//! tracing. Metrics are the lowest-cardinality view and work best for alerting and
//! long-term trend analysis.
//!
//! Snapshot ordering is intentional: the current [`SupervisorSnapshot`] is published
//! before the matching [`SupervisorEvent`] is broadcast, so event consumers can read
//! already-updated snapshot state.
//!
//! Nested event forwarding is best effort. If a nested supervisor's event receiver
//! lags behind, the runtime logs a warning and increments an observability drop
//! counter when the `metrics` feature is enabled.
//!
//! Shutdown and abort semantics are cooperative because supervised work runs as
//! Tokio tasks. A timed-out or aborted task is asked to stop, but non-yielding
//! futures are not forcibly preempted inside this crate. If you need hard stop
//! guarantees for blocking work, run that work in a dedicated blocking pool or
//! external process and supervise that boundary instead.
//!
//! Minimal setup:
//!
//! ```no_run
//! use tokio_supervisor::{ChildSpec, SupervisorBuilder};
//! use tracing_subscriber::FmtSubscriber;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let subscriber = FmtSubscriber::builder().finish();
//! tracing::subscriber::set_global_default(subscriber)?;
//!
//! let supervisor = SupervisorBuilder::new()
//!     .child(ChildSpec::new("worker", |ctx| async move {
//!         ctx.token.cancelled().await;
//!         Ok(())
//!     }))
//!     .build()?;
//!
//! let handle = supervisor.spawn();
//! let _events = handle.subscribe();
//! let _snapshot = handle.snapshot();
//! # handle.shutdown();
//! # let _ = handle.wait().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ```no_run
//! # #[cfg(feature = "metrics")]
//! # {
//! use metrics_exporter_prometheus::PrometheusBuilder;
//!
//! let _handle = PrometheusBuilder::new().install_recorder().unwrap();
//! # }
//! ```
//!
//! See the examples at:
//!
//! - `examples/subscribe_to_events.rs`
//! - `examples/subscribe_to_snapshots.rs`
//! - `examples/tracing.rs`
//! - `examples/metrics.rs` (requires `--features metrics`)

mod builder;
mod child;
mod context;
mod error;
mod event;
mod handle;
mod observability;
pub mod prelude;
mod restart;
mod runtime;
mod shutdown;
mod snapshot;
mod strategy;
mod supervisor;

pub use builder::SupervisorBuilder;
pub use child::{BoxError, ChildResult, ChildSpec};
pub use context::{ChildContext, SupervisorToken};
pub use error::{BuildError, ControlError, SupervisorError, SupervisorExit};
pub use event::{EventPathSegment, ExitStatusView, SupervisorEvent};
pub use handle::SupervisorHandle;
pub use restart::{BackoffPolicy, Restart, RestartIntensity};
pub use shutdown::{ShutdownMode, ShutdownPolicy};
pub use snapshot::{
    ChildMembershipView, ChildSnapshot, ChildStateView, SupervisorSnapshot, SupervisorStateView,
};
pub use strategy::Strategy;
pub use supervisor::Supervisor;
