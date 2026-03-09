# Tokio Supervisor Spec as of `2fe64fe`

This document describes the codebase as implemented at commit
`2fe64feff2d853c570569f899474844740e7077b`.

## Scope

`tokio-supervisor` is a Tokio library for structured task supervision with:

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
* restart intensity limiting with optional backoff
* lifecycle event subscriptions
* no detached child tasks owned by the supervisor runtime

The implementation is aimed at a conservative v0.1 surface rather than a full OTP-style runtime.

## Public API

The crate exports:

```rust
use tokio_supervisor::{
    BackoffPolicy, BoxError, BuildError, ChildContext, ChildResult, ChildSpec,
    ExitStatusView, Restart, RestartIntensity, ShutdownMode, ShutdownPolicy,
    Strategy, Supervisor, SupervisorBuilder, SupervisorError, SupervisorEvent,
    SupervisorExit, SupervisorHandle,
};
```

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

Each child is defined by a restartable factory closure:

```rust
impl ChildSpec {
    pub fn new<F, Fut>(id: impl Into<String>, f: F) -> Self
    where
        F: Fn(ChildContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChildResult> + Send + 'static;

    pub fn restart(self, restart: Restart) -> Self;
    pub fn shutdown(self, policy: ShutdownPolicy) -> Self;

    pub fn id(&self) -> &str;
    pub fn restart_policy(&self) -> Restart;
    pub fn shutdown_policy(&self) -> &ShutdownPolicy;
}
```

Defaults:

* restart policy: `Restart::Transient`
* shutdown policy: `ShutdownPolicy::default()`

### Restart and strategy types

```rust
pub enum Restart {
    Permanent,
    Transient,
    Temporary,
}

pub enum Strategy {
    OneForOne,
    OneForAll,
}
```

Restart intensity:

```rust
pub struct RestartIntensity {
    pub max_restarts: usize,
    pub within: Duration,
    pub backoff: BackoffPolicy,
}

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

Defaults:

* `Strategy::OneForOne`
* `RestartIntensity { max_restarts: 5, within: 30s, backoff: BackoffPolicy::None }`

### Shutdown types

```rust
pub struct ShutdownPolicy {
    pub grace: Duration,
    pub mode: ShutdownMode,
}

pub enum ShutdownMode {
    Cooperative,
    CooperativeThenAbort,
    Abort,
}
```

Default:

```rust
ShutdownPolicy {
    grace: Duration::from_secs(5),
    mode: ShutdownMode::CooperativeThenAbort,
}
```

### Builder, supervisor, and handle

```rust
impl SupervisorBuilder {
    pub fn new() -> Self;
    pub fn strategy(self, strategy: Strategy) -> Self;
    pub fn restart_intensity(self, intensity: RestartIntensity) -> Self;
    pub fn child(self, child: ChildSpec) -> Self;
    pub fn build(self) -> Result<Supervisor, BuildError>;
}

impl Supervisor {
    pub async fn run(self) -> Result<SupervisorExit, SupervisorError>;
    pub fn spawn(self) -> SupervisorHandle;
}

impl SupervisorHandle {
    pub fn shutdown(&self);
    pub async fn wait(&self) -> Result<SupervisorExit, SupervisorError>;
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SupervisorEvent>;
}
```

Build validation:

* no empty child list
* no duplicate child IDs
* restart intensity rejects zero `within`
* `BackoffPolicy::Fixed` rejects zero delay
* `BackoffPolicy::Exponential` rejects zero `base`, `factor`, or `max`

## Runtime behavior

### Child lifecycle

Each child is spawned from its stored factory closure, not from a one-shot future. The runtime tracks:

* logical child ID
* restart policy
* shutdown policy
* current generation
* child cancellation token
* abort handle
* running state

Generation rules:

* first run is generation `0`
* each restart increments generation by `1`

### Exit classification

Internal child exit states are:

* `Completed`
* `Failed(BoxError)`
* `Panicked`
* `Aborted`

`JoinError::is_cancelled()` is treated as `Aborted`. Other join errors are treated as `Panicked`.

Public event payloads expose:

```rust
pub enum ExitStatusView {
    Completed,
    Failed(String),
    Panicked,
    Aborted,
}
```

### Restart policy semantics

| Restart   | Completed | Failed | Panicked | Aborted |
| --------- | --------- | ------ | -------- | ------- |
| Permanent | yes       | yes    | yes      | yes     |
| Transient | no        | yes    | yes      | yes     |
| Temporary | no        | no     | no       | no      |

During shutdown, no restarts are scheduled.

### `OneForOne`

If a child exit is restartable:

* record the restart in the sliding restart window
* fail the supervisor with `RestartIntensityExceeded` if the limit is exceeded
* apply configured backoff
* restart only that child

If an exit is not restartable, the child stays stopped.

### `OneForAll`

If a child exit is restartable:

* record the restart in the sliding restart window
* fail the supervisor with `RestartIntensityExceeded` if the limit is exceeded
* apply configured backoff
* cancel the current group
* drain the old generation according to child shutdown policies
* create a fresh group cancellation token
* respawn all non-`Temporary` children

The runtime does not overlap old and new generations during a group restart.

### Restart intensity and backoff

Restart accounting uses a sliding window of restart timestamps.

* if recorded restarts within `within` exceed `max_restarts`, supervisor execution fails with `SupervisorError::RestartIntensityExceeded`
* `BackoffPolicy::None` uses zero delay
* `BackoffPolicy::Fixed` always uses the configured delay
* `BackoffPolicy::Exponential` multiplies from `base` by `factor` and caps at `max`

Shutdown preempts a pending restart delay, including zero-delay restarts.

### Shutdown semantics

External shutdown:

* transitions the supervisor to stopping
* emits `SupervisorStopping`
* cancels the group token and child tokens
* drains children according to each child's shutdown policy
* emits `SupervisorStopped` on successful shutdown
* returns `SupervisorExit::Shutdown`

Drain rules:

* `Abort`: abort immediately
* `Cooperative`: wait up to the shared grace deadline; if still running, abort, drain, and return `SupervisorError::ShutdownTimedOut(ids)`
* `CooperativeThenAbort`: wait up to the shared grace deadline, then abort and continue shutdown successfully

The runtime uses a group-wide deadline equal to the maximum grace among active cooperative children.

The same drain logic is used for one-for-all restarts before a new generation starts.

### Natural exit semantics

If all children stop without an external shutdown request:

* return `SupervisorExit::Completed` if the latest terminal status for every child is clean
* return `SupervisorExit::Failed` if the latest terminal status for any child is `Failed`, `Panicked`, or `Aborted`

Drained failures from superseded generations do not poison a later clean natural exit.

## Events

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

`SupervisorStopped` is emitted for both successful external shutdown and successful natural completion.

## Concurrency guarantees

The runtime is built around a single supervisor-owned `JoinSet`.

Properties that hold at this commit:

* all supervised tasks are owned by the runtime
* `wait()` does not resolve until child tasks have been joined and dropped
* one-for-all restarts drain the old generation before spawning the next one
* shutdown wins over a pending restart

## Current crate layout

```text
src/
├── builder.rs
├── child.rs
├── context.rs
├── error.rs
├── event.rs
├── handle.rs
├── lib.rs
├── restart.rs
├── shutdown.rs
├── strategy.rs
├── supervisor.rs
└── runtime/
    ├── child_factory.rs
    ├── child_runtime.rs
    ├── exit.rs
    ├── intensity.rs
    ├── mod.rs
    ├── shutdown.rs
    ├── spawn.rs
    └── supervision.rs
```

## Verified behavior

The following are covered by the test suite and examples in this commit:

* builder validation
* one-for-one restart behavior
* one-for-all restart behavior
* temporary children skipped during group restart
* no overlap between one-for-all generations
* panic handling
* restart intensity failure
* fixed restart backoff
* external shutdown and idempotent `shutdown()`
* `Abort` and `CooperativeThenAbort`
* child lifetime drained before `wait()` resolves
* shutdown winning over immediate restart
* event subscriptions via `SupervisorHandle::subscribe()`
