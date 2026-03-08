# Handoff Spec: Tokio Supervisor Library

## 0. Goal

Implement a Rust library that provides **structured task supervision** on top of Tokio, with:

* supervised child tasks
* restart strategies:

  * `OneForOne`
  * `OneForAll`
* restart policies:

  * `Permanent`
  * `Transient`
  * `Temporary`
* cooperative shutdown via `CancellationToken`
* forceful fallback via task abort
* restart intensity limiting
* event stream / observability
* no detached tasks created by the supervisor runtime

The implementation should be ergonomic for library users, but internally conservative and explicit.

---

## 1. Product constraints

### 1.1 Runtime target

* Tokio
* `tokio-util` for `CancellationToken`
* stable Rust

### 1.2 Core principle

Every child spawned by the supervisor must be:

* owned by the supervisor
* observable by the supervisor
* joined/drained before the supervisor finishes

No fire-and-forget children.

### 1.3 Non-goals for v0.1

Do not implement:

* nested supervisors as first-class child specs
* dynamic add/remove child at runtime
* readiness protocol
* health probes
* distributed supervision
* actor runtime / mailboxes

Design so these can be added later.

---

## 2. Public API target

The crate should feel like:

```rust
use tokio_supervisor::{
    ChildContext, ChildResult, ChildSpec, Restart, Strategy,
    SupervisorBuilder, ShutdownMode, ShutdownPolicy,
};
```

Example target UX:

```rust
let supervisor = SupervisorBuilder::new()
    .strategy(Strategy::OneForOne)
    .child(
        ChildSpec::new("worker-a", |ctx| async move {
            loop {
                tokio::select! {
                    _ = ctx.token.cancelled() => return ChildResult::Completed,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                }
            }
        })
        .restart(Restart::Transient)
    )
    .build()?;

let handle = supervisor.spawn();
handle.shutdown();
let exit = handle.wait().await?;
```

---

## 3. Core behavior requirements

### 3.1 Child model

Each child is defined by a **restartable factory** rather than a single future instance.

Each child has:

* logical ID
* restart policy
* shutdown policy
* factory closure
* generation counter managed by supervisor

A child instance is identified by:

* `id`
* `generation`

---

### 3.2 Child execution contract

Child closure signature should be effectively:

```rust
Fn(ChildContext) -> Fut
where
    Fut: Future<Output = ChildResult> + Send + 'static
```

`ChildContext` must include:

* `id`
* `generation`
* `token` (child cancellation token)
* `supervisor_token` (group token or clone)

Children are expected to cooperate with cancellation.

---

### 3.3 Child completion model

Public child return type:

```rust
pub enum ChildResult {
    Completed,
    Failed(BoxError),
}
```

Where:

```rust
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
```

Supervisor-internal terminal classification:

```rust
pub enum ExitStatus {
    Completed,
    Failed(BoxError),
    Panicked,
    Aborted,
}
```

Do not expose Tokio `JoinError` directly in the normal child result path.

---

### 3.4 Restart policy semantics

```rust
pub enum Restart {
    Permanent,
    Transient,
    Temporary,
}
```

Semantics table:

| Restart   | Completed | Failed | Panicked | Aborted |
| --------- | --------- | ------ | -------- | ------- |
| Permanent | yes       | yes    | yes      | yes     |
| Transient | no        | yes    | yes      | yes     |
| Temporary | no        | no     | no       | no      |

Restart policies only apply during normal supervisor operation. During shutdown, no restarts are scheduled regardless of policy.

---

### 3.5 Group strategy semantics

```rust
pub enum Strategy {
    OneForOne,
    OneForAll,
}
```

### OneForOne

If child exit is restartable, restart only that child.

### OneForAll

If child exit is restartable:

* stop all currently running children
* drain all old-generation tasks
* create a fresh group generation context
* restart all child specs

No overlapping generations during one-for-all restart.

---

### 3.6 Shutdown policy semantics

```rust
pub struct ShutdownPolicy {
    pub grace: Duration,
    pub mode: ShutdownMode,
}
```

```rust
pub enum ShutdownMode {
    Cooperative,
    CooperativeThenAbort,
    Abort,
}
```

Default:

* `ShutdownMode::CooperativeThenAbort`
* grace around a few seconds

Semantics:

* `Cooperative`: cancel token and wait; if timeout, shutdown returns error
* `CooperativeThenAbort`: cancel token, wait, then abort if still running
* `Abort`: immediately abort

For v0.1, graceful waiting may be group-oriented rather than per-child fine-grained, but the public type should still be per-child.

---

### 3.7 Restart intensity limiting

```rust
pub struct RestartIntensity {
    pub max_restarts: usize,
    pub within: Duration,
    pub backoff: BackoffPolicy,
}
```

```rust
pub enum BackoffPolicy {
    None,
    Fixed(Duration),
    Exponential {
        base: Duration,
        factor: u32,
        max: Duration,
    },
}
```

Required behavior:

* keep restart timestamps in a sliding window
* if restartable exits exceed threshold, supervisor fails with `RestartIntensityExceeded`
* apply backoff before restart

For v0.1, group-level accounting is acceptable.

---

## 4. Public API details

### 4.1 `ChildSpec`

Required public API:

```rust
pub struct ChildSpec { /* opaque or semi-opaque */ }

impl ChildSpec {
    pub fn new<F, Fut>(id: impl Into<String>, f: F) -> Self
    where
        F: Fn(ChildContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChildResult> + Send + 'static;

    pub fn restart(self, restart: Restart) -> Self;
    pub fn shutdown_policy(self, policy: ShutdownPolicy) -> Self;

    pub fn id(&self) -> &str;
    pub fn restart_policy(&self) -> Restart;
    pub fn shutdown_policy(&self) -> &ShutdownPolicy;
}
```

Internally, erase the closure into a trait object.

---

### 4.2 `SupervisorBuilder`

```rust
pub struct SupervisorBuilder { /* fields private */ }

impl SupervisorBuilder {
    pub fn new() -> Self;
    pub fn strategy(self, strategy: Strategy) -> Self;
    pub fn restart_intensity(self, intensity: RestartIntensity) -> Self;
    pub fn child(self, child: ChildSpec) -> Self;
    pub fn build(self) -> Result<Supervisor, BuildError>;
}
```

Validation at build time:

* no duplicate child IDs
* at least one child
* valid restart intensity

---

### 4.3 `Supervisor`

```rust
pub struct Supervisor { /* opaque */ }

impl Supervisor {
    pub async fn run(self) -> Result<SupervisorExit, SupervisorError>;
    pub fn spawn(self) -> SupervisorHandle;
}
```

Semantics:

* `run(self)` runs inline and does not return until supervisor exits. Since it consumes `self`, there is no way to trigger shutdown or subscribe to events from the caller. Use `run()` only for fire-and-wait-for-natural-completion scenarios. Callers who need shutdown control or event subscriptions should use `spawn()` instead.
* `spawn(self)` spawns the supervisor task and returns a handle

---

### 4.4 `SupervisorHandle`

```rust
#[derive(Clone)]
pub struct SupervisorHandle { /* opaque */ }

impl SupervisorHandle {
    pub fn shutdown(&self);
    pub async fn wait(&self) -> Result<SupervisorExit, SupervisorError>;
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SupervisorEvent>;
}
```

`shutdown()` must be idempotent.

`wait()` may be implemented as a shared oneshot or watch-based completion future.

---

### 4.5 Public errors and exits

```rust
pub enum BuildError {
    DuplicateChildId(String),
    EmptyChildren,
    InvalidConfig(&'static str),
}
```

```rust
pub enum SupervisorError {
    RestartIntensityExceeded,
    ShutdownTimedOut(String),
    Internal(String),
}
```

```rust
pub enum SupervisorExit {
    Shutdown,
    Completed,
    Failed,
}
```

---

### 4.6 Public events

```rust
pub enum SupervisorEvent {
    SupervisorStarted,
    SupervisorStopping,
    SupervisorStopped,
    ChildStarted {
        id: String,
        generation: u64,
    },
    ChildExited {
        id: String,
        generation: u64,
        status: ExitStatusView,
    },
    ChildRestartScheduled {
        id: String,
        generation: u64,
        delay: Duration,
    },
    ChildRestarted {
        id: String,
        old_generation: u64,
        new_generation: u64,
    },
    GroupRestartScheduled {
        delay: Duration,
    },
    RestartIntensityExceeded,
}
```

Since `BoxError` is awkward in event streams, define a user-facing status view:

```rust
pub enum ExitStatusView {
    Completed,
    Failed(String),
    Panicked,
    Aborted,
}
```

Stringify errors for event emission.

---

## 5. Internal architecture

### 5.1 Core internal types

Implement these internal concepts.

### `ChildFactory`

Trait-object-safe erased factory:

```rust
pub(crate) trait ChildFactory: Send + Sync + 'static {
    fn make(&self, ctx: ChildContext) -> ChildFuture;
}
```

```rust
pub(crate) type ChildFuture =
    Pin<Box<dyn Future<Output = ChildResult> + Send + 'static>>;
```

Use a blanket impl over closure wrapper type.

---

### `ChildEnvelope`

Children should return explicit identity info to supervisor:

```rust
pub(crate) struct ChildEnvelope {
    pub id: String,
    pub generation: u64,
    pub result: ChildResult,
}
```

Spawn wrapper should convert user child future into `ChildEnvelope`.

---

### `ChildRuntime`

Tracks runtime state for each spec:

```rust
pub(crate) struct ChildRuntime {
    pub spec: std::sync::Arc<ChildSpecInner>,
    pub generation: u64,
    pub state: RuntimeChildState,
    pub active_token: Option<CancellationToken>,
}
```

```rust
pub(crate) enum RuntimeChildState {
    Starting,
    Running,
    Stopping,
    Stopped,
}
```

---

### `SupervisorRuntime`

Owns active execution:

```rust
pub(crate) struct SupervisorRuntime {
    pub strategy: Strategy,
    pub intensity: RestartIntensity,
    pub state: SupervisorState,
    pub group_token: CancellationToken,
    pub join_set: tokio::task::JoinSet<ChildEnvelope>,
    pub children: std::collections::HashMap<String, ChildRuntime>,
    pub restart_times: std::collections::VecDeque<tokio::time::Instant>,
    pub events: tokio::sync::broadcast::Sender<SupervisorEvent>,
    pub shutdown_rx: tokio::sync::watch::Receiver<bool>,
}
```

May also include:

* completion sender
* tracing fields
* group epoch if desired

---

### 5.2 Spawning model

Implement helper:

```rust
fn spawn_child(&mut self, id: &str) -> Result<(), SupervisorError>;
```

Behavior:

1. look up child runtime/spec
2. increment generation
3. derive child token from current group token
4. build `ChildContext`
5. wrap child future into envelope-returning task
6. spawn into `JoinSet`
7. mark child `Running`
8. emit `ChildStarted`

Important:

* generation 0 = first run of a child
* increment generation before each *restart* spawn (not the initial spawn)
* so generation 0 = first run, generation 1 = first restart, generation 2 = second restart, etc.
* be consistent and test it

---

### 5.3 Supervision loop

Core loop:

```rust
loop {
    tokio::select! {
        changed = shutdown_rx.changed() => { ... }
        maybe = join_set.join_next() => { ... }
    }
}
```

Behavior:

* if external shutdown observed: perform shutdown path and exit
* if `join_next()` returns `None`: determine whether supervisor is done
* if child exits: classify and handle according to strategy and restart rules

Keep all restart decisions serialized inside this loop.

---

### 5.4 Join result classification

When `join_set.join_next()` returns:

* `Ok(ChildEnvelope { ... })`

  * map `ChildResult::Completed` → `ExitStatus::Completed`
  * map `ChildResult::Failed(err)` → `ExitStatus::Failed(err)`

* `Err(join_err)`

  * if `join_err.is_cancelled()` → `ExitStatus::Aborted`
  * if `join_err.is_panic()` → `ExitStatus::Panicked`
  * otherwise treat as internal failure

To know which child panicked/aborted, you should not rely solely on raw `join_next()`.

Recommended implementation:

* use `JoinSet::join_next_with_id()` (stable since tokio 1.45)
* maintain a runtime mapping from task ID to `(child_id, generation)`

---

### 5.5 Restart decision function

Implement pure helper:

```rust
fn should_restart(restart: Restart, status: &ExitStatus) -> bool;
```

This makes policy testable.

---

### 5.6 Backoff and intensity

Implement helpers:

```rust
fn prune_restart_window(&mut self, now: Instant);
fn record_restart(&mut self, now: Instant);
fn intensity_exceeded(&self) -> bool;
fn compute_backoff(&self, restart_count_in_window: usize) -> Duration;
```

Behavior:

* prune old timestamps outside `within`
* record before actual restart
* if threshold exceeded, emit event and fail supervisor

For `Exponential`:

* `delay = min(base * factor^k, max)`

Cap safely to avoid overflow.

---

### 5.7 One-for-one flow

On child exit:

1. update runtime state to stopped
2. emit `ChildExited`
3. evaluate restartability
4. if not restartable:

   * leave child stopped
   * if no children remain running and no restarts pending, supervisor may complete
5. if restartable:

   * intensity check
   * compute backoff
   * emit `ChildRestartScheduled`
   * sleep backoff
   * respawn just that child
   * emit `ChildRestarted`

---

### 5.8 One-for-all flow

On restartable child exit:

1. emit `ChildExited`
2. intensity check
3. compute backoff
4. emit `GroupRestartScheduled`
5. cancel current group token
6. gracefully drain/abort all running children according to shutdown mode
7. ensure `join_set` is empty
8. create fresh group token
9. respawn all children
10. emit `ChildRestarted` or `ChildStarted` events for new generations

Important:

* do not start new group before old tasks are drained
* all children use the fresh group token
* Temporary children that had previously completed normally are **not** restarted during a group restart — skip them. Only Permanent and Transient children are respawned.

---

### 5.9 Shutdown flow

Implement:

```rust
async fn shutdown_all(&mut self) -> Result<SupervisorExit, SupervisorError>;
```

Behavior:

1. transition state to `Stopping`
2. emit `SupervisorStopping`
3. cancel group token
4. wait for tasks to exit within grace window
5. abort remaining tasks if policy allows
6. drain `join_set`
7. emit `SupervisorStopped`
8. return `SupervisorExit::Shutdown`

For v0.1, it is acceptable to use a conservative group-wide shutdown deadline equal to:

* max grace among active children

Per-child precision can come later.

---

## 6. Crate layout

Recommended crate layout:

```text
tokio_supervisor/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── builder.rs
    ├── child.rs
    ├── context.rs
    ├── error.rs
    ├── event.rs
    ├── handle.rs
    ├── restart.rs
    ├── shutdown.rs
    ├── strategy.rs
    ├── supervisor.rs
    ├── runtime/
    │   ├── mod.rs
    │   ├── child_factory.rs
    │   ├── child_runtime.rs
    │   ├── exit.rs
    │   ├── intensity.rs
    │   ├── supervision.rs
    │   ├── spawn.rs
    │   └── shutdown.rs
    └── tests/
        ├── mod.rs
        ├── one_for_one.rs
        ├── one_for_all.rs
        ├── shutdown.rs
        ├── intensity.rs
        └── panic.rs
```

---

## 7. Module responsibilities

### `lib.rs`

Public re-exports only.

Export:

* `BoxError`
* `ChildContext`
* `ChildResult`
* `ChildSpec`
* `Restart`
* `Strategy`
* `ShutdownMode`
* `ShutdownPolicy`
* `RestartIntensity`
* `BackoffPolicy`
* `Supervisor`
* `SupervisorBuilder`
* `SupervisorHandle`
* `BuildError`
* `SupervisorError`
* `SupervisorExit`
* `SupervisorEvent`
* `ExitStatusView`

Keep internal runtime private.

---

### `child.rs`

Public child-facing types:

* `ChildSpec`
* `ChildResult`
* child builder methods
* internal `ChildSpecInner`

Should contain closure erasure glue or delegate to `runtime/child_factory.rs`.

---

### `context.rs`

Public `ChildContext`.

---

### `restart.rs`

Public:

* `Restart`
* `RestartIntensity`
* `BackoffPolicy`

Maybe helper defaults.

---

### `shutdown.rs`

Public:

* `ShutdownMode`
* `ShutdownPolicy`

Default impls live here.

---

### `strategy.rs`

Public `Strategy`.

---

### `event.rs`

Public:

* `SupervisorEvent`
* `ExitStatusView`

Helper conversion from internal exit state.

---

### `error.rs`

Public:

* `BuildError`
* `SupervisorError`
* `SupervisorExit`

Use `thiserror` if desired.

---

### `builder.rs`

Implementation of `SupervisorBuilder`.

This should mostly validate and assemble immutable config for `Supervisor`.

---

### `supervisor.rs`

Public `Supervisor` type.

Should be a thin wrapper over runtime config and spawn/run entry points.

---

### `handle.rs`

`SupervisorHandle` plus shared control and completion channels.

Likely contains:

* shutdown sender
* completion receiver
* event broadcast receiver creation

---

### `runtime/mod.rs`

Internal runtime exports.

---

### `runtime/child_factory.rs`

Factory trait and closure erasure.

---

### `runtime/child_runtime.rs`

Runtime child state structs.

---

### `runtime/exit.rs`

Internal exit classification:

* `ExitStatus`
* maybe `ClassifiedExit`

---

### `runtime/intensity.rs`

Sliding-window restart tracking and backoff calculation.

Keep pure/testable.

---

### `runtime/spawn.rs`

Child spawn wrapper logic.

---

### `runtime/shutdown.rs`

Graceful/group shutdown helpers.

---

### `runtime/supervision.rs`

Main supervision loop.

This is the heart of the runtime.

---

## 8. Suggested type signatures

These do not need to be exact, but should be close.

### `ChildContext`

```rust
pub struct ChildContext {
    pub id: String,
    pub generation: u64,
    pub token: tokio_util::sync::CancellationToken,
    pub supervisor_token: tokio_util::sync::CancellationToken,
}
```

### `ChildResult`

```rust
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

pub enum ChildResult {
    Completed,
    Failed(BoxError),
}
```

### `ChildSpec`

```rust
pub struct ChildSpec {
    inner: std::sync::Arc<ChildSpecInner>,
}
```

### `Supervisor`

```rust
pub struct Supervisor {
    config: SupervisorConfig,
}
```

### `SupervisorHandle`

```rust
#[derive(Clone)]
pub struct SupervisorHandle {
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    done_rx: tokio::sync::watch::Receiver<Option<Result<SupervisorExit, SupervisorError>>>,
    events_tx: tokio::sync::broadcast::Sender<SupervisorEvent>,
}
```

Use a `watch` channel initialized with `None` for the completion signal. The supervisor writes `Some(result)` on exit. Multiple callers of `wait()` can observe the result via `watch::Receiver::wait_for(|v| v.is_some())`. Since `watch` preserves the last value, all cloned handles see the result regardless of when they call `wait()`.

---

## 9. Dependency recommendations

Reasonable dependencies:

```toml
[dependencies]
tokio = { version = "1.45", features = ["rt", "rt-multi-thread", "sync", "time", "macros"] }
tokio-util = { version = "0.7", features = ["rt"] }
thiserror = "2"
tracing = "0.1"
pin-project-lite = "0.2"
```

Optional:

* `futures-core`
* `futures-util`

Avoid unnecessary dependencies.

---

## 10. Implementation order

### Phase 1: core types

Implement:

* `Restart`
* `Strategy`
* `ShutdownPolicy`
* `ChildContext`
* `ChildResult`
* `BuildError`
* `SupervisorError`
* `SupervisorExit`
* `SupervisorEvent`

### Phase 2: child factory and builder

Implement:

* closure-erased child factory
* `ChildSpec`
* `SupervisorBuilder`
* validation

### Phase 3: runtime basics

Implement:

* runtime config
* child spawn wrapper
* `JoinSet` ownership
* supervisor loop
* one-for-one restart
* external shutdown
* `spawn()` and `run()`

### Phase 4: one-for-all

Implement:

* group cancellation
* drain old tasks
* respawn all children

### Phase 5: restart intensity / backoff

Implement:

* sliding window
* backoff
* failure on threshold exceed

### Phase 6: observability

Implement:

* broadcast events
* tracing spans / logs

### Phase 7: hardening

Implement:

* panic tests
* abort tests
* race/shutdown idempotence tests

---

## 11. Testing matrix

### 11.1 Builder tests

* empty children rejected
* duplicate IDs rejected
* valid build succeeds

### 11.2 One-for-one tests

* failed transient child restarts
* permanent child restarts after completion
* temporary child does not restart
* siblings remain running

### 11.3 One-for-all tests

* failure of one restartable child restarts whole group
* old tasks are drained before new tasks start
* group token replaced correctly
* generations increment

### 11.4 Shutdown tests

* external shutdown stops all children
* shutdown is idempotent
* cooperative child exits cleanly
* stubborn child is aborted in `CooperativeThenAbort`

### 11.5 Panic tests

* child panic is detected
* transient child panics cause restart
* one-for-all panic restarts all

### 11.6 Intensity tests

* repeated failures exceed threshold
* supervisor exits with `RestartIntensityExceeded`
* backoff delays are applied

### 11.7 Structured concurrency tests

* no child outlives supervisor completion
* `join_set` drained before exit
* handle wait resolves exactly once

---

## 12. Recommended defaults

Implement `Default` for:

### `ShutdownPolicy`

```rust
ShutdownPolicy {
    grace: Duration::from_secs(5),
    mode: ShutdownMode::CooperativeThenAbort,
}
```

### `RestartIntensity`

```rust
RestartIntensity {
    max_restarts: 5,
    within: Duration::from_secs(30),
    backoff: BackoffPolicy::None,
}
```

### `SupervisorBuilder`

* default strategy: `OneForOne`
* default restart intensity: above

### `Restart`

```rust
Restart::Transient
```

### `ChildSpec`

* default restart policy: `Restart::default()` (`Transient`)
* default shutdown policy: `ShutdownPolicy::default()`

These are sane and unsurprising.

---

## 13. Important implementation notes

### 13.1 Do not detach tasks

Never drop a spawned child without joining/draining it.

### 13.2 Avoid embedding non-cloneable completion futures directly in handle API

Use a completion channel that supports `wait()` ergonomically and safely.

### 13.3 Use explicit identity envelopes

Do not rely solely on task IDs when you can carry logical identity directly.

### 13.4 Separate internal and public exit types

Keep `ExitStatus` internal; expose `ExitStatusView` in events.

### 13.5 Keep the supervisor loop single-threaded logically

All restart/shutdown state transitions should happen in one control loop.

### 13.6 Be conservative around sleeps/backoff

If shutdown arrives during backoff, shutdown should win.

So backoff waiting should be:

```rust
tokio::select! {
    _ = tokio::time::sleep(delay) => { /* restart */ }
    _ = group_or_shutdown_token.cancelled() => { /* stop */ }
}
```

or equivalent with the supervisor shutdown channel.

---

## 14. Nice-to-have documentation examples

Include examples for:

* simple one-for-one worker restart
* one-for-all coupled pipeline
* shutdown with `CancellationToken`
* listening to events with `subscribe()`

---

## 15. Stretch goals after v0.1

Do not implement now, but keep code extensible for:

* nested supervisors
* dynamic child add/remove
* required vs optional children
* readiness barrier
* per-child restart intensity
* jittered exponential backoff
* supervisor-as-child adapter

---

## 16. Minimal milestone definition

The library is “done enough” for v0.1 when:

* users can define children with closures
* build a supervisor
* run or spawn it
* observe restarts for one-for-one and one-for-all
* shut it down cleanly
* subscribe to lifecycle events
* rely on no detached child tasks
* get deterministic behavior under tests

---

# Proposed initial crate layout with file sketches

## `src/lib.rs`

```rust
pub mod builder;
pub mod child;
pub mod context;
pub mod error;
pub mod event;
pub mod handle;
pub mod restart;
pub mod shutdown;
pub mod strategy;
pub mod supervisor;

mod runtime;

pub use builder::SupervisorBuilder;
pub use child::{BoxError, ChildResult, ChildSpec};
pub use context::ChildContext;
pub use error::{BuildError, SupervisorError, SupervisorExit};
pub use event::{ExitStatusView, SupervisorEvent};
pub use handle::SupervisorHandle;
pub use restart::{BackoffPolicy, Restart, RestartIntensity};
pub use shutdown::{ShutdownMode, ShutdownPolicy};
pub use strategy::Strategy;
pub use supervisor::Supervisor;
```

## `src/context.rs`

```rust
use tokio_util::sync::CancellationToken;

pub struct ChildContext {
    pub id: String,
    pub generation: u64,
    pub token: CancellationToken,
    pub supervisor_token: CancellationToken,
}
```

## `src/restart.rs`

```rust
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    Permanent,
    Transient,
    Temporary,
}

#[derive(Debug, Clone)]
pub enum BackoffPolicy {
    None,
    Fixed(Duration),
    Exponential {
        base: Duration,
        factor: u32,
        max: Duration,
    },
}

#[derive(Debug, Clone)]
pub struct RestartIntensity {
    pub max_restarts: usize,
    pub within: Duration,
    pub backoff: BackoffPolicy,
}
```

## `src/shutdown.rs`

```rust
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    Cooperative,
    CooperativeThenAbort,
    Abort,
}

#[derive(Debug, Clone)]
pub struct ShutdownPolicy {
    pub grace: Duration,
    pub mode: ShutdownMode,
}
```

## `src/strategy.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    OneForOne,
    OneForAll,
}
```

## `src/error.rs`

```rust
#[derive(Debug)]
pub enum BuildError {
    DuplicateChildId(String),
    EmptyChildren,
    InvalidConfig(&'static str),
}

#[derive(Debug, Clone)]
pub enum SupervisorError {
    RestartIntensityExceeded,
    ShutdownTimedOut(String),
    Internal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorExit {
    Shutdown,
    Completed,
    Failed,
}
```

## `src/event.rs`

```rust
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum ExitStatusView {
    Completed,
    Failed(String),
    Panicked,
    Aborted,
}

#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    SupervisorStarted,
    SupervisorStopping,
    SupervisorStopped,
    ChildStarted {
        id: String,
        generation: u64,
    },
    ChildExited {
        id: String,
        generation: u64,
        status: ExitStatusView,
    },
    ChildRestartScheduled {
        id: String,
        generation: u64,
        delay: Duration,
    },
    ChildRestarted {
        id: String,
        old_generation: u64,
        new_generation: u64,
    },
    GroupRestartScheduled {
        delay: Duration,
    },
    RestartIntensityExceeded,
}
```

## `src/child.rs`

```rust
use std::{future::Future, sync::Arc};

use crate::{context::ChildContext, restart::Restart, shutdown::ShutdownPolicy};

pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

pub enum ChildResult {
    Completed,
    Failed(BoxError),
}

pub struct ChildSpec {
    pub(crate) inner: Arc<crate::runtime::child_factory::ChildSpecInner>,
}

impl ChildSpec {
    pub fn new<F, Fut>(id: impl Into<String>, f: F) -> Self
    where
        F: Fn(ChildContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChildResult> + Send + 'static,
    {
        todo!()
    }

    pub fn restart(self, restart: Restart) -> Self {
        todo!()
    }

    pub fn shutdown_policy(self, policy: ShutdownPolicy) -> Self {
        todo!()
    }

    pub fn id(&self) -> &str {
        todo!()
    }

    pub fn restart_policy(&self) -> Restart {
        todo!()
    }

    pub fn shutdown_policy(&self) -> &ShutdownPolicy {
        todo!()
    }
}
```

## `src/builder.rs`

```rust
use crate::{child::ChildSpec, error::BuildError, restart::RestartIntensity, strategy::Strategy, supervisor::Supervisor};

pub struct SupervisorBuilder {
    strategy: Strategy,
    restart_intensity: RestartIntensity,
    children: Vec<ChildSpec>,
}

impl SupervisorBuilder {
    pub fn new() -> Self {
        todo!()
    }

    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    pub fn restart_intensity(mut self, intensity: RestartIntensity) -> Self {
        self.restart_intensity = intensity;
        self
    }

    pub fn child(mut self, child: ChildSpec) -> Self {
        self.children.push(child);
        self
    }

    pub fn build(self) -> Result<Supervisor, BuildError> {
        todo!()
    }
}
```

## `src/supervisor.rs`

```rust
use crate::{error::{SupervisorError, SupervisorExit}, handle::SupervisorHandle};

pub struct Supervisor {
    pub(crate) config: crate::runtime::SupervisorConfig,
}

impl Supervisor {
    pub async fn run(self) -> Result<SupervisorExit, SupervisorError> {
        todo!()
    }

    pub fn spawn(self) -> SupervisorHandle {
        todo!()
    }
}
```

## `src/handle.rs`

```rust
use crate::{error::{SupervisorError, SupervisorExit}, event::SupervisorEvent};

#[derive(Clone)]
pub struct SupervisorHandle {
    pub(crate) inner: std::sync::Arc<crate::runtime::HandleInner>,
}

impl SupervisorHandle {
    pub fn shutdown(&self) {
        todo!()
    }

    pub async fn wait(&self) -> Result<SupervisorExit, SupervisorError> {
        todo!()
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SupervisorEvent> {
        todo!()
    }
}
```

## `src/runtime/mod.rs`

```rust
pub mod child_factory;
pub mod child_runtime;
pub mod exit;
pub mod intensity;
pub mod supervision;
pub mod shutdown;
pub mod spawn;

pub(crate) use child_factory::ChildSpecInner;
pub(crate) use child_runtime::{ChildRuntime, RuntimeChildState};
pub(crate) use exit::ExitStatus;

pub(crate) struct SupervisorConfig {
    pub strategy: crate::strategy::Strategy,
    pub restart_intensity: crate::restart::RestartIntensity,
    pub children: Vec<crate::child::ChildSpec>,
}

pub(crate) struct HandleInner {
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub done_rx: tokio::sync::watch::Receiver<Option<Result<crate::error::SupervisorExit, crate::error::SupervisorError>>>,
    pub events_tx: tokio::sync::broadcast::Sender<crate::event::SupervisorEvent>,
}
```

## `src/runtime/child_factory.rs`

```rust
use std::{future::Future, pin::Pin};

use crate::{child::ChildResult, context::ChildContext, restart::Restart, shutdown::ShutdownPolicy};

pub(crate) type ChildFuture = Pin<Box<dyn Future<Output = ChildResult> + Send + 'static>>;

pub(crate) trait ChildFactory: Send + Sync + 'static {
    fn make(&self, ctx: ChildContext) -> ChildFuture;
}

pub(crate) struct ChildSpecInner {
    pub id: String,
    pub restart: Restart,
    pub shutdown: ShutdownPolicy,
    pub factory: Box<dyn ChildFactory>,
}
```

## `src/runtime/child_runtime.rs`

```rust
use tokio_util::sync::CancellationToken;

pub(crate) enum RuntimeChildState {
    Starting,
    Running,
    Stopping,
    Stopped,
}

pub(crate) struct ChildRuntime {
    pub spec: std::sync::Arc<crate::runtime::child_factory::ChildSpecInner>,
    pub generation: u64,
    pub state: RuntimeChildState,
    pub active_token: Option<CancellationToken>,
}
```

## `src/runtime/exit.rs`

```rust
pub(crate) enum ExitStatus {
    Completed,
    Failed(crate::child::BoxError),
    Panicked,
    Aborted,
}
```

## `src/runtime/intensity.rs`

```rust
use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;

use crate::restart::{BackoffPolicy, RestartIntensity};

pub(crate) struct RestartTracker {
    pub intensity: RestartIntensity,
    pub restarts: VecDeque<Instant>,
}

impl RestartTracker {
    pub fn new(intensity: RestartIntensity) -> Self {
        todo!()
    }

    pub fn prune(&mut self, now: Instant) {
        todo!()
    }

    pub fn record(&mut self, now: Instant) {
        todo!()
    }

    pub fn exceeded(&self) -> bool {
        todo!()
    }

    pub fn next_backoff(&self) -> Duration {
        todo!()
    }
}
```

## `src/runtime/spawn.rs`

```rust
pub(crate) struct ChildEnvelope {
    pub id: String,
    pub generation: u64,
    pub result: crate::child::ChildResult,
}
```

## `src/runtime/shutdown.rs`

Helpers for draining and abort fallback.

## `src/runtime/supervision.rs`

Main runtime loop implementation.

---

# Suggested first three milestones for Codex

## Milestone 1

Get this working end-to-end:

* one child
* no restarts
* clean shutdown
* `spawn()` + `wait()`

## Milestone 2

Add:

* multiple children
* `OneForOne`
* restart policies
* event stream

## Milestone 3

Add:

* `OneForAll`
* restart intensity
* backoff
* panic/abort tests

---

# Final guidance for the implementation

Prioritize:

* correctness
* structured concurrency guarantees
* predictable restart semantics
* clean tests

Do not over-generalize v0.1. A smaller, well-tested API is better than a very abstract one.

The most important invariant is:

**the supervisor must always know the state of every child it created, and must not finish until all such children are drained.**
