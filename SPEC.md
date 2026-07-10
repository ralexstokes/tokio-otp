# tokio-otp — specification

This document specifies the observable behavior and public API contracts of
the workspace, at a level where an independent implementation could rebuild
it. It describes *what* the library guarantees, not *how* the code achieves
it.

The workspace is four crates:

- **`tokio-supervisor`** — structured task supervision for tokio, standalone
  and independently usable.
- **`tokio-otp`** — the product crate: a typed actor layer plus a runtime
  layer that supervises actors, built on `tokio-supervisor`.
- **`tokio-otp-derive`** — the `#[derive(Topology)]` proc macro, re-exported
  by `tokio-otp` under its default `derive` feature; not used directly.
- **`tokio-otp-console`** — experimental web dashboard for supervision trees.

---

## 1. Purpose and design pillars

Erlang/OTP-style fault tolerance for tokio: supervision trees restart failed
tasks under configurable policies, and typed actors exchange messages through
references that survive restarts. Four pillars carry the whole design:

**P1 — Policy-driven supervision.** A supervisor owns a set of child tasks and
reacts to each exit according to per-child restart policy, supervisor strategy,
and a rate limit (restart intensity). Supervisors nest to form trees with
independent restart scopes.

**P2 — Restart-stable actor references.** An `ActorRef<M>` is bound to an
actor *identity*, not to a live task. Sends transparently follow the actor
across restarts; they fail only when no future incarnation is expected.

**P3 — Consistent observability ordering.** State snapshots are published
*before* the corresponding lifecycle event is broadcast, so an event handler
reading the snapshot always sees state at least as new as the event.

**P4 — Cooperative shutdown everywhere.** All cancellation is cooperative at
await points, driven by `CancellationToken`s, bounded by grace periods, with
abort as a policy-controlled fallback. Nothing forcibly preempts a non-yielding
future, and the docs say so.

Two cross-cutting principles shape the API surface:

- **Typed refs are the only actor addresses; strings are labels.** No actor
  API accepts a string as an address — there is no registry and no lookup.
  Actor ids exist for humans: tracing, stats, snapshots, the console, and
  supervisor child ids. Name-based discovery, where an application wants it,
  is a userland pattern (an ordinary directory actor holding
  `HashMap<String, ActorRef<M>>`). The supervisor plane keeps string child
  ids deliberately: children are untyped, the tree is a named structure, and
  a bad name can only fail loudly, never confuse types.
- **Actors are the only unit of execution; a graph is only a wiring spec.**
  There is no whole-graph run mode. Execution happens by driving individual
  `RunnableActor`s — normally as supervisor children. Failure isolation and
  fate-sharing are pure supervisor policy (`OneForOne` isolates, `OneForAll`
  or a nested subtree shares fate), never an execution mode.

---

## 2. `tokio-supervisor`

### 2.1 Core types and construction

- `BoxError = Box<dyn std::error::Error + Send + Sync + 'static>`;
  `ChildResult = Result<(), BoxError>`. Both are identical to (and
  interchangeable with) `tokio_otp::BoxError` / `ActorResult`.
- `ChildSpec::new(id, factory)` pairs a child id with an async factory
  `Fn(ChildContext) -> Future<Output = ChildResult> + Send + Sync + 'static`.
  The factory is re-invoked on every (re)start. Builder-style setters:
  `.restart(RestartPolicy)`, `.shutdown(ShutdownPolicy)`,
  `.restart_intensity(RestartIntensity)` (per-child override); accessors
  `id()`, `restart_policy()`, `shutdown_policy()`. The inner state is
  reference-counted: ChildSpecs are cheaply cloneable and reusable.
- `SupervisorSpec::new(Supervisor)` carries the same three policy setters for
  a *nested-supervisor* child. `From<Supervisor> for SupervisorSpec` applies
  the defaults, so APIs that nest supervisors accept
  `impl Into<SupervisorSpec>`.
- `SupervisorBuilder` collects children (`.child(ChildSpec)`,
  `.supervisor(id, impl Into<SupervisorSpec>)`) plus `strategy`, a default
  `restart_intensity`, `control_channel_capacity` (default 64), and
  `event_channel_capacity` (default 256). Children start in insertion order.
- `build()` validates: unique, non-empty child ids; non-zero channel
  capacities; valid intensity/backoff (non-zero window and delays, non-zero
  factor) for the default and every per-child override. Errors:
  `SupervisorBuildError::{DuplicateChildId(String), InvalidConfig(&'static str)}`.
  **Zero children is valid** — the supervisor starts idle and is populated
  dynamically.
- A built `Supervisor` is cloneable; each clone is an independent
  configuration that can be spawned separately (child specs are shared by
  ref-count).

### 2.2 Policies

- `RestartPolicy`: `Always` (always restart), `OnFailure` (default; restart on
  `Err`/panic/abort, not on `Ok`), `Never` (never restart; runs at most
  once).
- `Strategy`: `OneForOne` (default; only the failed child restarts),
  `OneForAll` (every unexpected exit stops and restarts the whole group;
  `Never` children are drained with the group but not respawned; the old
  generation is fully drained before the new one spawns — no overlap), or
  `RestForOne` (the failed child and children declared after it are drained,
  then eligible children in that suffix restart in declaration order).
- `RestartIntensity { max_restarts: usize, within: Duration, backoff }`
  (default 5 in 30 s, no backoff; `new(max_restarts, within)` +
  `.with_backoff(policy)`): a sliding window of restart timestamps. On each
  restart, timestamps older than `within` are evicted; if the remaining count
  *exceeds* `max_restarts`, the supervisor exits with
  `SupervisorError::RestartIntensityExceeded`. Per-child overrides track
  their own window.
- `BackoffPolicy`: `None` (default) | `Fixed(d)` |
  `Exponential { base, factor, max }` (delay = `base * factor^attempt`,
  clamped to `max`) | `JitteredExponential { .. }` (same progression, each
  delay uniformly jittered into `[d/2, d]` — equal jitter). The attempt count
  is a per-child **consecutive-restart counter**, reset once an incarnation
  runs longer than the intensity window (`within`); it is independent of the
  sliding window's timestamp eviction, so delays never shrink merely because
  old timestamps aged out.
- `ShutdownPolicy { grace: Duration, mode: ShutdownMode }`, default
  `CooperativeThenAbort` with 5 s grace; constructors `new(grace, mode)`,
  `cooperative_strict(grace)`, `cooperative_then_abort(grace)`, `abort()`
  (zero grace). Modes:
  - `CooperativeStrict` — cancel the child's token, wait up to `grace`, then
    abort the task *and report a timeout error*;
  - `CooperativeThenAbort` — same, but the fallback abort is not an error;
  - `Abort` — abort the tokio task immediately.

  When several cooperative children drain at once (shutdown, `OneForAll`
  group restart), one shared deadline applies = the maximum grace among them;
  grace periods never compound serially. All modes remain cooperative at
  tokio poll boundaries — a non-yielding future is never forcibly preempted
  (P4); hard-stop guarantees require isolating blocking work outside the
  supervised task.

### 2.3 Running and lifecycle

- `Supervisor::spawn() -> SupervisorHandle` (background tokio task) is the
  only way to run a supervisor; foreground driving is `spawn()` +
  `handle.wait()`.
- Children spawn in insertion order but initialize concurrently — readiness
  is not gated. Dependency ordering self-resolves at the actor layer because
  sends wait on mailbox bindings (P2). Each (re)start invokes the factory
  with a fresh `ChildContext` exposing `id()`, `generation()` (starts at 0,
  increments per restart), `shutdown_token()` (this child's own
  `CancellationToken`), and `supervisor_token()` — a `SupervisorToken`
  read-only view of supervisor-wide cancellation (`cancelled().await`,
  `is_cancelled()`).
- Exit classification: `Ok` ⇒ Completed; `Err` ⇒ Failed; panic ⇒ Panicked;
  task aborted ⇒ Aborted. All but Completed count as failure for restart
  policy.
- **No natural exit**: the supervisor is a service. Once every child has
  finished (completed or terminally failed without restart) it idles at zero
  running children, keeps serving control commands, and continues until
  explicit shutdown or an intensity escalation. Per-child outcomes remain
  visible as `ChildSnapshot::last_exit`; run-to-completion is a caller
  pattern (`wait_for_snapshot` on an all-stopped predicate + `shutdown()`),
  not an engine mode.
- Explicit shutdown: `handle.shutdown()` is non-blocking and idempotent;
  cancellation wins over any pending restart delay, including zero-delay
  restarts. If cooperative-mode children time out during shutdown, `wait()`
  returns `Err(SupervisorError::ShutdownTimedOut(ids))` (the string lists the
  timed-out child ids). `shutdown_and_wait()` combines the two.
- `handle.wait()` resolves `Ok(())` after a graceful shutdown, or
  `Err(SupervisorError)` on intensity escalation / shutdown timeout
  (`SupervisorError::{RestartIntensityExceeded, ShutdownTimedOut(String),
  Internal(String)}` — `Internal` indicates a runtime bug). Safe to call from
  any number of handle clones concurrently: the first caller joins the tokio
  task, the rest share the result through a watch channel. A successful
  return means all child tasks are joined.
- **RAII shutdown**: dropping the last handle clone requests graceful
  shutdown, exactly as `shutdown()` would. A tree's lifetime is tied to the
  last reference to it; fire-and-forget operation = keep a handle alive. This
  is also what makes nested supervision abort-safe: aborting a
  nested-supervisor child drops its inner handle binding, which gracefully
  stops the subtree.

### 2.4 Dynamic children

- `add_child(ChildSpec)` / `remove_child(id)` /
  `add_supervisor(id, impl Into<SupervisorSpec>)` are async and await
  control-channel capacity (there are no `try_` variants and no "busy"
  error). Adds validate like `build()` (duplicate id, empty id, invalid
  intensity) and start the child immediately.
- `remove_child` stops the child per its shutdown policy before resolving.
  Errors: `ControlError::{DuplicateChildId, UnknownChildId,
  ChildRemovalInProgress (concurrent duplicate removal), InvalidConfig,
  SupervisorStopping, ShutdownTimedOut(id), Unavailable (supervisor task
  gone / control channel closed), Internal}`. Removing the last child is
  valid; the supervisor idles at zero children.
- There is no path addressing: to mutate a nested supervisor, obtain its
  stable handle (§2.5) and call `add_child`/`remove_child` on *it*. Handles
  are `Clone + Send` scoped capabilities — capture one in actor state or
  deliver it by message to control the tree from inside an actor.

### 2.5 Nested supervisors

- Nesting is a first-class child kind: `SupervisorBuilder::supervisor(id,
  sup)` statically, `SupervisorHandle::add_supervisor(id, sup)` dynamically;
  restart/shutdown/intensity policies attach via `SupervisorSpec` as for any
  child. Contract:
  - parent cancellation (or abort of the nested child) ⇒ graceful shutdown of
    the nested tree (via the RAII rule of §2.3);
  - nested exit maps to the child result: graceful shutdown ⇒ `Ok`,
    escalation or internal error ⇒ `Err` — so the parent's restart policy
    applies. A nested subtree never exits on its own (§2.3), so a nested
    child's "completion" is always one of these two;
  - nested lifecycle events surface in the parent's event stream wrapped in
    `SupervisorEvent::Nested { id, generation, event }` (recursively;
    `event.path()` yields the `EventPathSegment { id, generation }` chain and
    `event.leaf()` the innermost event); forwarding is best-effort — a
    lagging forwarder drops events with a warning and a metric;
  - the nested tree's snapshot appears (coalesced, eventually consistent)
    under the parent's `ChildSnapshot::supervisor` field.
- Mechanism (one, explicit): the parent passes a parent link (event sink,
  snapshot cell, id+generation) into each incarnation of the nested
  supervisor at (re)spawn. No ambient state: a supervisor child behaves
  identically no matter what spawns it.
- **Restart-stable nested handles**: the parent owns per-nested-child stable
  channels (event broadcast, snapshot watch, control slot) that each
  incarnation binds into. `SupervisorHandle::supervisor(id) ->
  Option<SupervisorHandle>` returns the stable handle of a direct nested
  supervisor — keep it, pass it around; subscriptions and snapshots on it
  survive nested restarts. Between incarnations, control operations on a
  stable handle fail fast with `ControlError::Unavailable` (and `wait()`
  errors) rather than queueing.

### 2.6 Observability

- `handle.subscribe()` → bounded broadcast of `SupervisorEvent`, exactly 10
  variants: `SupervisorStarted`, `SupervisorStopping`, `SupervisorStopped`
  (terminal; emitted after all child tasks are joined), `Nested { id,
  generation, event }`, `ChildStarted { id, generation }`,
  `ChildRemoved { id }`, `ChildExited { id, generation, status }`,
  `ChildRestartScheduled { id, generation, delay }` (also emitted for the
  triggering child of a `OneForAll` group restart),
  `ChildRestarted { id, old_generation, new_generation }`,
  `RestartIntensityExceeded`. `status` is an `ExitStatusView`:
  `Completed | Failed(String — the error's Display) | Panicked | Aborted`.
  Slow subscribers observe `Lagged` and skip missed events.
  Removal-in-progress is snapshot state (`membership: Removing`), not an
  event.
- `handle.snapshot()` / `subscribe_snapshots()` → clone / `watch::Receiver`
  of `SupervisorSnapshot { state: Running|Stopping|Stopped, strategy,
  children }`; `ChildSnapshot { id, generation, state
  (Starting|Running|Stopping|Stopped), membership (Active|Removing),
  last_exit: Option<ExitStatusView>, restart_count, next_restart_in:
  Option<Duration>, supervisor: Option<Box<SupervisorSnapshot>> }`. Lookup
  helpers `child(id)` and `descendant(path)` (walks child ids through nested
  snapshots) exist on both snapshot types.
- **Ordering guarantee (P3): the snapshot is updated before the
  corresponding event is broadcast.**
- `handle.event_sender()` exposes the broadcast sender, for consumers that
  need many independent receivers (the console subscribes once per WebSocket
  connection).
- `handle.monitor_restart(id) -> Result<RestartMonitor, RestartMonitorError>`
  captures the child's current generation **synchronously at call time** and
  returns an `IntoFuture` that resolves with the new generation once the
  child is observed `Running` with a greater one. Accessors `id()`,
  `baseline_generation()`. Errors (`UnknownChild`, `ChildRemoved`,
  `SupervisorStopped`) if the child is unknown/removed or the supervisor
  stops first; a resolved restart wins over a simultaneously-terminal
  snapshot. Direct children only. Call it *before* triggering the crash.
- Prelude extension traits: `SupervisorEventReceiverExt::wait_for_event(
  predicate)` on `broadcast::Receiver<SupervisorEvent>` (discards
  non-matching events) and `SupervisorSnapshotReceiverExt::wait_for_snapshot(
  predicate)` on `watch::Receiver<SupervisorSnapshot>` (checks the current
  value first). "Wait until all children run" is the documented
  all-`Running` snapshot predicate.
- `tracing`: the supervisor runs in an `info_span!("supervisor")` and each
  child in an `info_span!("child")`, carrying `supervisor_name`,
  `supervisor_path`, `strategy`, `child_id`, and `generation` fields; every
  lifecycle event is also logged with a `root.`-prefixed dot-path.
- `metrics` (feature): `supervisor.children.running` gauge;
  `supervisor.children.started`, `supervisor.children.exited`,
  `supervisor.restarts`, `supervisor.restart_intensity_exceeded`,
  `supervisor.events.dropped`, `supervisor.shutdown_timeouts` counters;
  `supervisor.child_shutdown.duration` histogram — labeled by
  supervisor/path/strategy.
- `serde` (feature): `Serialize`/`Deserialize` on the snapshot and event view
  types.

---

## 3. Actor layer of `tokio-otp`

The actor layer lives in a private `actor/` module tree; its API is
re-exported flat from the crate root.

### 3.1 Actor model

- `BoxError` / `ActorResult = Result<(), BoxError>` — the same types as the
  supervisor's `BoxError` / `ChildResult`.
- `RawActor` trait (the custom-loop escape hatch): `type Msg: Send + 'static`
  plus `async fn run(&self, ctx: ActorContext<Self::Msg>) -> ActorResult` —
  full control of the receive loop. Actor values are
  `Clone + Send + Sync + 'static`; cloned per run. Deliberately not
  implementable by bare closures: an actor is a named type, keeping the
  message type visible at the definition site and the state explicit. Every
  registration surface (`GraphBuilder`, Topology fields, the dynamic factory,
  `RuntimeHandle::add_actor`) is bounded on `RawActor`.
- `Actor` trait (the recommended surface):
  `async fn handle(&mut self, msg: Self::Msg, ctx: &ActorContext<Self::Msg>)
  -> ActorResult`, optional `on_start` / `on_stop` hooks, and
  `drain_policy()`. A blanket `impl<A: Actor> RawActor for A` provides the
  receive loop: clone the actor, `on_start` once, then messages in mailbox
  order, then `on_stop`. `on_stop` also runs after a drain but is skipped
  when `handle` or `on_start` returned an error; an `on_start` error fails
  the run like a `handle` error (an ordinary restartable failure under
  supervision).
- `DrainPolicy` (`#[non_exhaustive]`): `Discard` (default) stops at shutdown
  dropping queued messages (queued `call`s observe
  `CallError::ReplyDropped`); `Drain` closes the mailbox to new sends,
  handles everything already queued, then stops. Drains are not separately
  time-bounded — the surrounding shutdown timeout is the backstop — and must
  tolerate `SendError` from already-stopped siblings (§3.8: shutdown is
  concurrent).
- **Restart = state reset.** Each incarnation clones the wiring-time actor
  value; `Clone` is the reset mechanism. Per-incarnation resources
  (connections, files, sessions) are acquired in `on_start` — the OTP `init`
  idiom. State that must survive restarts belongs behind shared handles (an
  `Arc`, a database, another actor's `ActorRef`), which cloning shares
  instead of resetting.
- **No internal supervision or execution.** The actor layer defines actors
  and their wiring; it does not run graphs. Each actor executes as one
  incarnation at a time via `RunnableActor::run_until` (§3.4), normally
  hosted as a supervisor child. What an exit means — restart, final
  completion, escalation — is entirely the host's policy.

### 3.2 Graph construction

- `GraphBuilder` (registration methods take `&mut self`; refs are minted
  immediately so they can be captured by other actors' state):
  - `add(actor)` — id is the unqualified type name (generics stripped), with
    `-2`/`-3`… deduplication for repeated types;
  - `actor(id, actor)` — explicit id;
  - `slot::<M>(id) -> (ActorSlot<M>, ActorRef<M>)` + `define(slot, actor)` —
    cyclic wiring: refs are minted before actor values exist. The slot token
    is deliberately non-`Clone`/non-`Copy`, so double-fill is unrepresentable;
    a message-type mismatch between slot and actor is a compile error; a
    token from a different builder is a build error.
  - Config: `name` (used in tracing fields; default: a generated unique
    `graph-N`), `mailbox_capacity` (default 64, applies to every actor),
    `actor_shutdown_timeout` (default 5 s; see §3.4).
- `build() -> Result<Graph, GraphBuildError>` validates, first error wins
  (registration-time errors are recorded in order and reported first):
  `EmptyGraph` (a graph must contain ≥ 1 actor), `DuplicateActorId`,
  `MissingActor` (a slot opened but never filled), `InvalidConfig` (empty
  actor id, empty graph name, zero mailbox capacity, foreign slot token).
- Actor ids are **labels, not addresses**: derive field names, type names, or
  explicit strings, consumed by tracing/stats/snapshots/console and as
  supervisor child ids — never by any addressing API.
- **Bounded mailboxes and cycles (documented contract):** backpressure
  propagates through `send`. In a cyclic graph this can deadlock: two actors
  that `send` to each other while both mailboxes are full wait forever, and a
  `call` cycle deadlocks at depth one (the callee cannot answer while the
  caller awaits the reply). Idioms: `try_send` on feedback edges; `call` only
  "downhill" along a DAG ordering.

### 3.3 `#[derive(Topology)]`

For a named-field struct `Pipeline` whose fields are actors, the derive
generates:

- a `PipelineRefs` struct with one field per topology field, typed
  `ActorRef<<FieldType as RawActor>::Msg>`;
- `Pipeline::graph(wire)` — builds with a default `GraphBuilder`; and
- `Pipeline::graph_with(builder, wire)` — accepts a preconfigured builder
  (name, capacities, extra hand-registered actors).

The `wire` closure receives `&PipelineRefs` *before any actor value is
constructed*, so cyclic topologies need no forward declarations: each field's
actor can capture any other field's ref. Field names become actor labels
verbatim. Visibility of the refs struct and generated methods follows the
topology struct; each refs field follows its topology field.

Compile-time guarantees: enums, unions, tuple structs, unit structs, generic
structs, and zero-field structs are rejected at derive time; a non-actor
field type fails with E0277 spanned at that field type; a message-type
mismatch in wiring fails to compile; double-filling a field is
unrepresentable. `graph`/`graph_with` still return `GraphBuildError` for the
remaining runtime checks (e.g. a `graph_with` builder that already registered
an actor under the same id as a field). Dynamic shapes — actors created in a
loop, runtime-chosen ids — use `GraphBuilder` directly.

### 3.4 Execution surface — `Graph` and `RunnableActor`

- A `Graph` (cheaply cloneable) is the wiring *and* the runnable set: `name()`,
  `actors()` — the `RunnableActor`s in declaration order — `stats()`
  (per-actor `ActorStats`, same order), and `dynamic_factory()` (§3.5).
  There is no whole-graph run mode; the only run guard is per-actor.
- `RunnableActor` (cheaply cloneable; shares the graph's long-lived binding
  slot, so refs keep working across independent restarts):
  - `label()`, `stats()`;
  - `run_until(shutdown: impl Future<Output = ()>, rebind: RebindPolicy)
    .await -> Result<(), ActorRunError>` — one incarnation: binds a fresh
    bounded mailbox for the duration, runs the actor (as a spawned task, in
    an `actor` tracing span). At most one concurrent run per actor —
    `ActorRunError::AlreadyRunning`; the guard is released when the run
    ends.
  - `terminate_binding()` — marks the binding terminated so senders fail
    fast with `ActorTerminated` instead of waiting for a rebind that will
    never come. A hand-written host must call it when it gives up on an
    actor whose rebind policy left the binding waiting (supervised hosting
    does this automatically; §4.3).
- Per-run shutdown sequence: when `shutdown` resolves, cancel the actor's
  token, wait up to `actor_shutdown_timeout`, then abort the straggling
  task. An abort during a *requested* shutdown is reported as success (with a
  `Cancelled` exit in observability); outside a requested shutdown, external
  cancellation of the actor task is an `ActorRunError::Failed`. An actor
  error returns `ActorRunError::Failed { actor_id, source }`; an actor panic
  resumes unwinding out of `run_until`, so a supervisor host observes a
  panic. Dropping the `run_until` future cancels the actor token and aborts
  the task.
- Binding disposition at run end. `RebindPolicy` (`Always` / `OnFailure` /
  `Never`, default `Never`) is a per-run argument — there is no pre-set
  mutable policy, so a stale policy is unrepresentable:
  - if shutdown was requested (or the run ended in a requested-shutdown
    state): **Terminated**, always;
  - otherwise `Always` leaves the binding **Unbound** (rebind expected) on
    any exit; `OnFailure` leaves it Unbound after failure/panic/cancellation
    and Terminated after a clean stop; `Never` always terminates.

### 3.5 Dynamic actor creation

`graph.dynamic_factory() -> RunnableActorFactory` mints additional runnable
actors at runtime sharing the graph's execution settings (observability
identity, mailbox capacity, shutdown timeout). `RunnableActorFactory::new()`
(also `Default`) constructs a standalone factory with the defaults.
`factory.actor(label, actor) -> (RunnableActor, ActorRef<A::Msg>)` — the ref
is typed at creation; no lookup, no downcast. Name-based discovery, where an
application wants it, is the userland `Directory<M>` pattern (§1).

### 3.6 `ActorRef`, mailboxes, `Reply` (P2)

- `ActorRef<M>` is cloneable and `Debug`; `id()` returns the label, `stats()`
  a point-in-time `ActorStats` (§3.9). It is bound to the actor's long-lived
  binding slot, which is in one of three states: **Unbound** (not yet
  started, or between restarts where a new mailbox is expected), **Bound**
  (live mailbox), **Terminated** (no future run expected).
- **Delivery contract: at-most-once.** Mailboxes are incarnation-owned and
  bounded; each run binds a fresh mailbox, and messages accepted by a dead
  incarnation are lost with it. The loss windows are restart and shutdown.
  `Ok` from a send means the message was accepted by the current
  incarnation's mailbox, not that it will be processed. Restarts losing the
  queue is deliberate: a mailbox that survived restarts would redeliver the
  poison message that caused the crash, converting one failure into a restart
  loop. Stronger guarantees (acks, redelivery) are user protocol built on
  `call`/`Reply`, not transport features.
- `send(msg).await -> Result<(), SendError>`: waits for a bound mailbox,
  waits for capacity, and rides through restart windows *when a rebind is
  expected* (after a stale mailbox closes, it waits for the binding to leave
  the stale mailbox and bind a fresh one, then retries with the same
  message). It errors with `SendError::ActorTerminated` only when the binding
  is Terminated or the binding source is gone. Cancelling a pending send
  drops the message.
- `try_send(msg)`: non-blocking; errors `SendError::{ActorNotRunning
  (Unbound), ActorTerminated, MailboxFull, MailboxClosed}` (all carry the
  actor id).
- `call(|reply| Msg::…).await -> Result<T, CallError>`: request/response via
  a one-shot `Reply<T>` constructed by the ref and embedded in the message by
  the caller-supplied closure. Waits like `send`. The actor answers with
  `reply.send(value)` (consumes the `Reply`; a vanished caller is ignored).
  `CallError::Send(SendError)` (with `From`) if delivery fails;
  `CallError::ReplyDropped` if the actor drops the `Reply` unanswered.
- `ActorContext<M>` (passed by value to `RawActor::run`, by shared reference
  to `Actor::handle`):
  - `id()`, `shutdown_token() -> &CancellationToken`, `is_shutting_down()`,
    `myself() -> ActorRef<M>` (a sender to this actor's own mailbox);
  - `recv().await -> Option<M>` — next message, **fail-fast at shutdown**: a
    biased check returns `None` as soon as shutdown is requested, even with
    messages still queued;
  - `try_recv() -> Result<M, tokio::sync::mpsc::error::TryRecvError>` —
    non-blocking, bypasses the shutdown check; intended for hand-written
    drain-then-exit loops (`Empty` does not prove the mailbox is drained
    while senders hold permits);
  - `run_blocking` (§3.7).

### 3.7 Blocking work

One method, no dedicated types:
`ctx.run_blocking(f: impl FnOnce(&CancellationToken) -> R + Send + 'static)
.await -> R` — runs `f` on tokio's blocking pool and waits for it.

- The token given to the closure is a child of the actor's shutdown token
  and is also cancelled if the `run_blocking` future is dropped. Cancellation
  is cooperative: long-running closures should poll `token.is_cancelled()`.
- A panic in `f` resumes on the actor task — an ordinary actor panic under
  supervision. The return value is otherwise opaque to the framework:
  blocking failures are whatever `R` encodes (typically the user's own
  `Result`), and the actor decides what they mean.
- Shutdown backstop: the actor awaits `run_blocking`, so the actor's
  `actor_shutdown_timeout` bounds a closure that ignores the token. After
  that timeout aborts the actor task, the blocking thread runs detached
  (tokio blocking tasks cannot be aborted once started).
- Detached / concurrent blocking work is a documented *pattern*, not API:
  clone `ctx.myself()`, call `tokio::task::spawn_blocking` directly, and
  send the outcome back as a message — the mailbox is the completion
  mechanism; bring your own semaphore for admission control.

### 3.8 Lifecycle-ordering contracts

- **Start and shutdown are concurrent, not OTP-sequential.** Supervised
  actors spawn in declaration order but initialize concurrently with no
  readiness gating; dependency ordering self-resolves because sends wait on
  bindings. Shutdown cancels all siblings concurrently under one shared
  max-grace deadline. Corollary: drain loops must tolerate `SendError` from
  already-stopped siblings (skip or log, don't propagate — propagating fails
  the draining actor itself).

### 3.9 Observability & inspection

- **Pull-based stats**: `ActorStats { actor_id, messages_received,
  messages_accepted, sends_rejected, mailbox_depth, mailbox_capacity }`.
  The three counters are relaxed atomics on the binding core — they
  accumulate for the lifetime of the binding and survive restarts;
  `messages_received` increments at the receive path, the send counters at
  each `send`/`try_send` outcome. Mailbox depth/capacity are computed on read
  from the bound sender (zero per-message cost; both zero while unbound).
  Available via `ActorRef::stats()`, `RunnableActor::stats()`,
  `Graph::stats()`, and `RuntimeHandle::actor_stats()`; incarnation data
  (generation, restart_count, last_exit) lives in the supervisor snapshot.
- **Tracing**: `actor` spans, lifecycle logs for actor and mailbox events,
  and per-message `trace!` events on send/receive (level-filterable, free
  when disabled).
- **No metrics backend in the actor layer.** Exporting time-series is a
  user-side sampler task over the stats/snapshot surfaces (documented
  pattern; ~10 lines). The crate's `metrics` feature only forwards to the
  supervisor's lifecycle metrics.

---

## 4. Runtime layer of `tokio-otp`

### 4.1 Composition modes

1. **Integrated runtime** (flagship): `Runtime::builder().graph(g)
   .strategy(…).restart(…).shutdown(…).restart_intensity(…).build()` — every
   graph actor becomes its own supervised child with uniform policies.
   Defaults: `OneForOne`, `OnFailure`, default shutdown policy, default
   intensity. `.graph()` is optional: a graph-less runtime starts empty and
   grows via `add_actor`. `build()` returns
   `Result<Runtime, SupervisorBuildError>`.
2. **Per-actor supervision, à la carte**: `SupervisedActors::new(graph)` with
   defaults (`.restart(…)`, `.shutdown(…)`) plus per-actor overrides keyed by
   typed ref — `actor_restart(&ref, …)`, `actor_shutdown(&ref, …)`,
   `actor_restart_intensity(&ref, …)` (typo-impossible; no unknown-actor
   error is representable) — then `build()` (a `Vec<ChildSpec>` for embedding
   in any supervisor tree), `build_supervisor(SupervisorBuilder)`, or
   `build_runtime(SupervisorBuilder)` (packages the result as a `Runtime`
   that retains the graph's dynamic factory).
3. Whole-graph fate-sharing is not a mode: "all actors restart together" is
   `SupervisedActors` + `Strategy::OneForAll` (or a nested subtree for scoped
   blast radius), which gives the same behavior with per-actor observability.

### 4.2 `Runtime` and `RuntimeHandle`

- `Runtime::new(supervisor)` wraps a raw supervisor (with a default actor
  factory); `Runtime::spawn() -> RuntimeHandle`; `into_supervisor()` is the
  escape hatch to the raw `Supervisor` for first-class nesting — it discards
  the actor factory, so `add_actor` is unavailable afterwards.
- `RuntimeHandle` (cloneable) is a full `SupervisorHandle` delegation —
  `shutdown`, `shutdown_and_wait`, `wait`, `add_child`, `remove_child`,
  `add_supervisor`, `supervisor(id)`, `subscribe`, `snapshot`,
  `subscribe_snapshots`, `monitor_restart` — plus:
  - `supervisor_handle()` — a clone of the underlying handle;
  - `add_actor(label, actor, DynamicActorOptions { restart, shutdown,
    restart_intensity }) -> Result<ActorRef<A::Msg>, ControlError>` — mints
    the runnable actor from the runtime's factory, adds a supervised child
    whose id is the label, records it for stats, and returns the typed ref.
    `DynamicActorOptions::default()` = `OnFailure`, default shutdown, no
    intensity override. Removal is plain `remove_child(label)` (which also
    drops the actor from the stats set on success or on a matching removal
    timeout);
  - `actor_stats() -> Vec<ActorStats>` over all actors created with this
    runtime (graph actors plus dynamic ones; a re-added label replaces the
    old entry);
  - `console()` (feature `console`) — a `ConsoleBuilder` pre-wired with the
    runtime's snapshot watch, event sender, and actor-stats pull source.
  - The RAII rule delegates: dropping the last `RuntimeHandle` clone requests
    graceful shutdown.

### 4.3 Actor-children glue (the crate's core job)

Each `RunnableActor` becomes a `ChildSpec` whose factory runs
`run_until(child_ctx.shutdown_token().cancelled(), rebind)`:

- the rebind policy mirrors the child's `RestartPolicy` variant-for-variant
  (`Always`, `OnFailure`, `Never`) — so ref liveness always matches what the
  supervisor will actually do;
- a drop guard on the child spec terminates the actor's binding when the
  supervisor discards the spec — the point after which no restart can happen
  (intensity exhausted, child removed, supervisor exit) — so senders fail
  fast with `ActorTerminated` instead of waiting forever for a rebind.

Hand-drivers using the public `run_until` own the same obligation (call
`terminate_binding` when giving up), documented on the method.

### 4.4 Prelude

`tokio_otp::prelude` re-exports the crate's whole actor + runtime surface
plus the documented `tokio_supervisor::prelude` contents (including both
extension traits). Exclusions: the duplicate `BoxError` (same type) and the
feature-gated console types (crate root only). A mirror test guards drift
against `tokio_supervisor::prelude`.

---

## 5. `tokio-otp-console` (experimental)

An axum server with WebSocket streaming and an embedded single-file
HTML/JS/CSS frontend that renders the supervision tree, child states, live
events, summary stats, and per-actor stats (message counts, mailbox depth).

- `Console::builder()`: `.snapshots(watch::Receiver<SupervisorSnapshot>)`
  (required), `.events(broadcast::Sender<SupervisorEvent>)` (required; each
  WebSocket connection subscribes its own receiver),
  `.actor_stats(fn() -> Vec<ActorStatsView>)` (optional pull source; default
  empty), `.bind(addr)` (default `127.0.0.1:9100`). `build()` panics if a
  required channel is missing.
- `console.spawn().await -> io::Result<ConsoleHandle>` (background;
  `local_addr()`, `shutdown()`), or `console.run().await` (foreground).
- `ActorStatsView` mirrors `ActorStats` field-for-field as a display type.
- Depends on `tokio-supervisor` with its `serde` feature (snapshot/event
  views are serialized to the frontend).

---

## 6. Feature flags and conventions

| Crate | Feature | Effect |
|---|---|---|
| tokio-supervisor | `metrics` | lifecycle metrics instruments (§2.6; the actor layer has none) |
| tokio-supervisor | `serde` | Serialize/Deserialize on snapshot/event views |
| tokio-otp | `derive` (default) | re-export `#[derive(Topology)]` |
| tokio-otp | `metrics` | forwards to `tokio-supervisor/metrics` |
| tokio-otp | `console` | re-export `Console`/`ConsoleBuilder`/`ConsoleHandle`/`ActorStatsView` + `RuntimeHandle::console()` |

Cross-crate identities: `BoxError` is the same type in both crates;
supervisor `ChildResult` and actor `ActorResult` are both
`Result<(), BoxError>`. `tokio-supervisor` is independently usable;
`tokio-otp` builds on it and is the product. Two preludes (supervisor; otp,
which re-exports it), one mirror test.
