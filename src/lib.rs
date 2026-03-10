//! Structured task supervision for Tokio, inspired by Erlang/OTP.
//!
//! `tokio-supervisor` manages the lifecycle of a group of async tasks
//! (*children*), automatically restarting them according to configurable
//! policies when they fail, panic, or are aborted. Supervisors can be nested
//! to form supervision trees with independent restart scopes.
//!
//! # Core concepts
//!
//! | Type | Role |
//! |------|------|
//! | [`SupervisorBuilder`] | Constructs and validates a supervisor. |
//! | [`Supervisor`] | A configured supervisor, ready to [`run`](Supervisor::run) or [`spawn`](Supervisor::spawn). |
//! | [`SupervisorHandle`] | Control and observe a running supervisor. |
//! | [`ChildSpec`] | Pairs an async factory with restart/shutdown policies. |
//! | [`ChildContext`] | Per-spawn context given to each child (id, generation, cancellation token). |
//!
//! # Strategies
//!
//! [`Strategy`] controls what happens when a child exits unexpectedly:
//!
//! - **[`OneForOne`](Strategy::OneForOne)** — only the failed child is
//!   restarted. Siblings are unaffected. This is the default.
//! - **[`OneForAll`](Strategy::OneForAll)** — all children are stopped and
//!   restarted together. [`Temporary`](Restart::Temporary) children are still
//!   drained with the group but are not respawned. Use this when children have
//!   hard interdependencies.
//!
//! # Restart policies
//!
//! Each child has a [`Restart`] policy:
//!
//! - **[`Permanent`](Restart::Permanent)** — always restarted, regardless of
//!   exit status.
//! - **[`Transient`](Restart::Transient)** (default) — restarted only on
//!   failure (`Err`, panic, or abort). A clean `Ok(())` exit is final.
//! - **[`Temporary`](Restart::Temporary)** — never restarted. Runs at most
//!   once.
//!
//! Restarts are bounded by a [`RestartIntensity`] limit (default: 5 restarts
//! within 30 seconds). When exceeded, the supervisor exits with
//! [`SupervisorError::RestartIntensityExceeded`]. An optional [`BackoffPolicy`]
//! inserts a delay before each restart attempt (fixed, exponential, or
//! jittered exponential). A shutdown request always wins over a pending
//! restart delay, including zero-delay restarts.
//!
//! # Shutdown
//!
//! Each child has a [`ShutdownPolicy`] that controls how it is stopped:
//!
//! - **[`Cooperative`](ShutdownMode::Cooperative)** — cancel the child's token
//!   and wait up to the grace period. If the child does not exit, a timeout
//!   error is reported.
//! - **[`CooperativeThenAbort`](ShutdownMode::CooperativeThenAbort)** (default,
//!   5 s grace) — cooperative with a fallback Tokio abort.
//! - **[`Abort`](ShutdownMode::Abort)** — abort the Tokio task immediately.
//!
//! When the supervisor is draining multiple cooperative children at once
//! (during shutdown or a [`OneForAll`](Strategy::OneForAll) restart), it uses a
//! shared deadline equal to the maximum grace period among the active
//! cooperative children.
//!
//! All shutdown modes are cooperative at Tokio poll boundaries. A non-yielding
//! future is never forcibly preempted. If you need hard-stop guarantees for
//! blocking work, isolate it in a dedicated blocking pool or external process
//! and supervise the boundary.
//!
//! # Dynamic children
//!
//! Children can be added and removed at runtime through the
//! [`SupervisorHandle`]:
//!
//! - [`add_child`](SupervisorHandle::add_child) /
//!   [`remove_child`](SupervisorHandle::remove_child) target the root
//!   supervisor.
//! - [`add_child_at`](SupervisorHandle::add_child_at) /
//!   [`remove_child_at`](SupervisorHandle::remove_child_at) target a nested
//!   supervisor by path.
//! - `try_` variants return [`ControlError::Busy`] immediately instead of
//!   waiting when the control channel is full.
//!
//! A supervisor must always have at least one child; removing the last active
//! child returns [`ControlError::LastChildRemovalUnsupported`].
//!
//! # Nested supervisors
//!
//! A [`Supervisor`] can be converted into a [`ChildSpec`] via
//! [`into_child_spec`](Supervisor::into_child_spec), allowing it to be
//! supervised as a child of another supervisor. The nested supervisor:
//!
//! - Forwards lifecycle events to the parent as
//!   [`SupervisorEvent::Nested`] wrappers.
//! - Publishes its snapshot into the parent's
//!   [`ChildSnapshot::supervisor`] field.
//! - Participates in the parent's control-plane registry, so
//!   [`add_child_at`](SupervisorHandle::add_child_at) can reach any depth.
//! - Is restarted by the parent according to the outer [`ChildSpec`]'s
//!   restart and shutdown policies.
//!
//! # Observability
//!
//! The crate provides three complementary observability channels:
//!
//! - **[`SupervisorEvent`] subscriptions** — the most precise choice when your
//!   code needs to react programmatically to lifecycle changes. Subscribe via
//!   [`SupervisorHandle::subscribe`].
//! - **`tracing` spans and logs** — automatic structured output for every
//!   lifecycle event. The supervisor runs inside an `info_span!("supervisor")`
//!   and each child inside an `info_span!("child")`, both carrying
//!   `supervisor_name`, `supervisor_path`, `child_id`, and `generation` fields.
//! - **`metrics` counters, gauges, and histograms** (requires the **`metrics`**
//!   feature) — lowest-cardinality view, best for dashboards and alerting.
//!   Emits `supervisor.children.running`, `supervisor.children.started`,
//!   `supervisor.children.exited`, `supervisor.restarts`,
//!   `supervisor.restart_intensity_exceeded`, `supervisor.events.dropped`,
//!   `supervisor.shutdown_timeouts`, and `supervisor.child_shutdown.duration`.
//!
//! ## Ordering guarantee
//!
//! The supervisor publishes an updated [`SupervisorSnapshot`] **before**
//! broadcasting the corresponding [`SupervisorEvent`], so event handlers can
//! read already-consistent snapshot state.
//!
//! ## Nested event forwarding
//!
//! Forwarding of nested supervisor events to the parent is best-effort. If a
//! nested supervisor's event receiver lags behind, the runtime logs a warning
//! and increments the `supervisor.events.dropped` counter (when the `metrics`
//! feature is enabled).
//!
//! # Quick start
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
//! # Cargo features
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `metrics` | no | Enables `metrics` crate integration for counters, gauges, and histograms. |
//!
//! # Examples
//!
//! - `examples/one_for_one_restart.rs` — basic restart behaviour.
//! - `examples/one_for_all_pipeline.rs` — interdependent children with
//!   `OneForAll`.
//! - `examples/nested_supervisor.rs` — supervision trees.
//! - `examples/dynamic_children.rs` — adding and removing children at runtime.
//! - `examples/per_child_restart_intensity.rs` — per-child intensity overrides.
//! - `examples/shutdown_with_cancellation_token.rs` — graceful shutdown driven
//!   by a signal.
//! - `examples/subscribe_to_events.rs` — reacting to lifecycle events.
//! - `examples/subscribe_to_snapshots.rs` — polling supervisor state.
//! - `examples/tracing.rs` — structured logging output.
//! - `examples/metrics.rs` — Prometheus metrics (requires `--features metrics`).

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
