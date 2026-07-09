# tokio-otp — clean-room specification & complexity ledger

This document specifies the observable behavior and public API contracts of
the workspace, at a level where an independent implementation could rebuild
it. It describes *what* the library guarantees, not *how* the code achieves
it. The review is complete: the **decision log (D1–D12)** plus the spec body
constitute the **target contract** for the refactor; where the body differs
from the current implementation, the delta is marked with its decision id.
(⚠ mechanism-leak flags from the original review have all been resolved by
decisions and removed.)

The closing **complexity ledger** maps each reviewed feature to the public
surface it added, the machinery that existed only to support it, and the
usage evidence; every entry now carries its resolution.

Scale reference (current implementation): ~12.4K lines of non-test source,
~7.3K lines of integration tests, across 5 crates.

---

## 0. Decision log

Resolved review decisions. The spec body below describes the **decided target
contract**; where it differs from the current implementation, the delta is
marked with the decision id.

- **D1 (2026-07-09) — supervisors always allow empty** (resolves L12).
  There is no `allow_empty` flag and no natural-completion exit: a supervisor
  is a service that runs until explicit shutdown or a restart-intensity
  escalation, idling at zero children. Zero-children builds and last-child
  removal are valid. Deleted with it: `SupervisorBuildError::EmptyChildren`,
  `ControlError::LastChildRemovalUnsupported`, the natural-exit branch, the
  shutdown-while-idle exit special case, the entire terminal-status
  bookkeeping ("superseded generations don't poison a clean exit"), and the
  `SupervisorExit::Completed`/`Failed` variants — `wait()` reduces to
  `Ok(())` on graceful shutdown vs `Err(SupervisorError)`. Run-to-completion
  becomes a user pattern (`wait_for_snapshot(all children Stopped)` + explicit
  `shutdown()`, reading per-child `last_exit` for failure detection); a
  `run_to_completion()` convenience may be added later if the pattern proves
  common. Known losses: nested batch subtrees no longer bubble completion to
  their parent automatically, and ~8 run-to-completion tests port to the
  snapshot pattern.

- **D2 (2026-07-09) — dropping the last handle requests graceful shutdown**,
  uniformly across `SupervisorHandle`, `RuntimeHandle`, and `GraphHandle`
  (`GraphHandle` clause later mooted by D4, which deletes the type).
  RAII: a tree's lifetime is tied to the last reference to it. Required by D1
  (a dropped-handle supervisor would otherwise be an immortal, unreachable
  zombie) and already load-bearing for abort-safety of nested supervision
  (aborting a nested-supervisor child drops its inner handle without calling
  `shutdown()`; the drop is what gracefully stops the subtree). For the
  supervisor this *documents existing accidental behavior* — the handle owns
  the only shutdown `watch::Sender` and the runtime treats the closed channel
  as a shutdown request — contradicting the current rustdoc, which claims
  drop does nothing; a regression test should pin it. For `GraphHandle` it is
  a small behavior change (today dropping its `CancellationToken` does not
  cancel). `RuntimeHandle` inherits the supervisor behavior by delegation.
  Fire-and-forget operation = keep the handle alive.

- **D3 (2026-07-09) — `Actor` is the handler-style trait; the raw trait is
  `RawActor`.** Renames: current `MessageHandler` → `Actor` (the recommended
  surface owns the prime name), current `Actor` → `RawActor` (the custom-loop
  escape hatch, following the `Waker`/`RawWaker` naming pattern). The blanket
  impl becomes `impl<A: Actor> RawActor for A`, and every bound that accepts
  "anything runnable as an actor" — `GraphBuilder::add`/`actor`/`define`,
  `#[derive(Topology)]` field types, `RunnableActorFactory::actor`,
  `RuntimeHandle::add_actor` — is expressed against `RawActor`.

- **D4 (2026-07-09) — actors are the only unit of execution; a graph is only
  a wiring spec** (resolves L7 and L8). Whole-graph execution is removed:
  `Graph::spawn`, `Graph::run_until`, `GraphHandle`, the internal graph drive
  loop, `SupervisedGraph`, and the graph-run failure semantics ("one actor
  exit fails the whole run" — `GraphError::ActorStopped` / `ActorFailed` /
  `ActorPanicked` / `ActorCancelled`) are all deleted. A `Graph` is built and
  then decomposed exactly once into runnable actors; the idle/running/
  decomposed tri-state (§3.3 mechanism leak) reduces to a one-shot decompose
  guard. Execution happens only by driving `RunnableActor`s — in practice as
  supervisor children via `tokio-otp`. Failure isolation and fate-sharing
  become pure supervisor policy: `OneForOne` isolates, `OneForAll` or a
  nested subtree shares fate (this replaces `SupervisedGraph`), and a clean
  early exit is ordinary `Transient`-child completion rather than a graph
  failure. Supersedes the `GraphHandle` clause of D2. Cost: `tokio-actor`
  alone no longer executes multi-actor graphs — it defines actors and wiring;
  a driver (normally `tokio-supervisor` via `tokio-otp`, or hand-driven
  `run_until` in tests) executes them.

- **D5 (2026-07-09) — blocking work is one method, zero types** (resolves
  L1). The sole facility is
  `ActorContext::run_blocking(f: impl FnOnce(&CancellationToken) -> R) -> R`:
  spawn on the tokio blocking pool, await, resume panics on the actor. The
  token is a child of the actor's shutdown token, also cancelled if the
  `run_blocking` future is dropped; the closure returns any `R` (typically
  the user's own `Result`) — the framework has no blocking-failure taxonomy.
  Deleted: all 9 blocking types (`BlockingContext`, `BlockingHandle`,
  `BlockingOptions`, `BlockingTaskId`, `BlockingOperationError`,
  `BlockingTaskError`, `BlockingTaskFailure`, `SpawnBlockingError`,
  `SharedError`), the admission semaphore and its two `GraphBuilder` knobs,
  `blocking_shutdown_timeout` (the actor's own shutdown timeout is the
  backstop), the claim-for-wait protocol, the completion-event channel, the
  reaper select-arm in every actor run, and per-task spans/metrics.
  Detached blocking work is a documented *pattern*, not machinery: clone
  `ctx.myself()`, `tokio::task::spawn_blocking`, send the outcome back as a
  message — the mailbox is the completion mechanism. Known loss (accepted):
  unobserved failure escalation — detached work that fails only matters if
  its result message says so.

- **D6 (2026-07-09) — actor visibility is a pull surface, not a metrics
  pipeline** (resolves L2). Each actor's binding core owns an `ActorStats`
  block of plain atomics — `messages_received` (incremented at the receive
  path), `messages_accepted` / `sends_rejected` (incremented by `ActorRef` on
  send outcomes) — plus mailbox depth/capacity computed on read from the
  sender half (zero per-message cost). Read via `ActorRef::stats()` and
  enumerable through `ActorSet` / the registry / `RuntimeHandle`, so the
  console can render queue depths and message counts alongside the
  supervision tree. Deleted: the `metrics` feature of `tokio-actor`, all
  per-message backend emits and the send-duration `Instant` timing, and the
  `Arc<OnceLock<GraphObservability>>` threading through
  bindings/refs/slots/registry (stats live in the binding refs already hold).
  Per-message `trace!` events and lifecycle logs stay. `tokio-supervisor`'s
  event-driven lifecycle metrics stay under its own `metrics` feature;
  `tokio-otp`'s `metrics` feature now forwards only to the supervisor.
  Accepted losses: send-duration histograms (mailbox depth is the
  backpressure signal; instrument your own send sites if you need latency),
  and self-exporting time-series — exporting is a documented ~10-line
  user-side sampler task over the stats/snapshot surface.

- **D7 (2026-07-09) — nesting is a first-class child kind with restart-stable
  handles; path addressing is deleted** (resolves L3 and L4). Nested
  supervisors are added via `SupervisorBuilder::supervisor(id, sup)` /
  `SupervisorHandle::add_supervisor(id, sup)` instead of
  `Supervisor::into_child_spec` closures. Because the parent knows the child
  is a supervisor, it passes an explicit `ParentLink` (event sink, snapshot
  cell, id+generation) at each (re)spawn — one explicit mechanism replacing
  the three task-local channels (event forwarder, coalescing snapshot bridge,
  control-scope registry); the forwarding contracts and P3 ordering are
  unchanged. Nested handles get the `ActorRef` treatment: the parent owns
  stable per-nested-child channels (event broadcast, snapshot watch, control
  slot) that each incarnation binds into, and `SupervisorHandle::supervisor(id)`
  returns a restart-stable handle for a direct nested supervisor —
  subscriptions and snapshots now survive nested restarts (impossible
  today). Dynamic control from anywhere = capture the right handle (they are
  `Clone + Send`, scoped capabilities) in actor state or deliver it by
  message; documented pattern for the post-`spawn()` handle hand-off.
  Deleted: `add_child_at` / `remove_child_at` / `try_*_at` on both handle
  types, `Supervisor::into_child_spec`, `NestedControlRegistry`, all three
  task-locals, and the "child behaves differently depending on which task
  spawns it" mechanism leak. `SupervisorEvent::Nested` + `path()`/`leaf()`
  remain for event consumers.

- **D8 (2026-07-09) — surface-trim batch** (resolves L5, L6, L9, L11, L14,
  L15):
  - **L5**: `try_add_child` / `try_remove_child` (both handle types) and
    `ControlError::Busy` are deleted; control operations await channel
    capacity.
  - **L6**: rebind policy becomes a per-run argument —
    `RunnableActor::run_until(shutdown, rebind: RebindPolicy)` — deleting the
    `set_rebind_policy` mutable side-channel; a stale/unset policy is now
    unrepresentable. `terminate_binding` stays (the host's drop guard still
    needs it when no further run will come). The `Restart` → `RebindPolicy`
    mapping remains in `tokio-otp`, now visible at the call site.
  - **L9**: `wait_until_running` deleted from both handles;
    `wait_for_snapshot` with an all-running predicate is the replacement
    (documented example).
  - **L11**: `ChildRemoveRequested` and `GroupRestartScheduled` deleted
    (consumers are display-only; removal-in-progress is visible as snapshot
    `membership: Removing` under the P3 ordering guarantee, and a `OneForAll`
    restart emits `ChildRestartScheduled` for the triggering child followed
    by per-child `ChildRestarted` events). 10 event variants remain.
  - **L14**: the exponential-backoff attempt count is redesigned as a plain
    consecutive-restart counter per child, reset when a run survives longer
    than the intensity window (`within`); it is no longer coupled to the
    sliding-window eviction, so delays no longer shrink as timestamps age
    out. Intensity accounting itself is unchanged.
  - **L15**: handle-less `Supervisor::run` is deleted; foreground driving is
    `spawn()` + `handle.wait()`, and D2 preserves the old "drop the future to
    tear down" property. `Runtime::into_supervisor` stays (it is how a
    runtime's tree is nested via `add_supervisor`).

- **D9 (2026-07-09) — actor addressing is typed refs end to end; ids are
  labels; the registry is deleted.** Principle: strings die where types
  exist; names survive where the domain is a human-facing tree. On the actor
  plane, no API accepts a string as an address: deleted are `ActorRegistry`,
  `RegistryError`, `LookupError`, the string-lookup `actor_ref(id)` methods,
  `ctx.registry()`, `RuntimeHandle::actor_ref`, and `remove_actor` (collapses
  into `remove_child`, since a dynamic actor's child id is its label).
  Dynamic creation returns the typed ref directly (`add_actor` already does);
  refs travel by message; name-based discovery, where wanted, is a documented
  userland pattern (a `Directory<M>` actor holding refs — self-hosting, typed,
  ~20 lines). `SupervisedActors` per-actor overrides are keyed by
  `&ActorRef<M>` instead of strings (`RuntimeBuildError::UnknownActor`
  deleted). Consequences: with no registry, every runtime supports
  `add_actor`, so `RuntimeBuilder::dynamic()`, `MissingGraph`, and
  `DynamicActorError` are deleted — `.graph()` is simply optional (completes
  D1's follow-on). Actor ids remain as *labels* only (derive field names,
  type names, or explicit), unique for display sanity, consumed by tracing,
  stats, snapshots, and the console — never by APIs. The supervisor plane
  keeps string child ids deliberately: children are untyped, the tree is a
  named structure, and a bad name can only fail loudly, never confuse types.

- **D10 (2026-07-09) — mailbox ownership and lifecycle-ordering contracts**
  (affirmations of current behavior, now explicit):
  - **Mailboxes are incarnation-owned.** Each run binds a fresh mailbox;
    messages accepted by a dead incarnation are lost with it. Delivery is
    **at-most-once**, with loss windows at restart and shutdown. Rationale: a
    persistent identity-owned mailbox would redeliver the poison message that
    caused the crash, converting one failure into a restart loop. Stronger
    guarantees (acks, redelivery) are user protocol built on `call`/`Reply`,
    not transport features.
  - **`Clone` is the reset mechanism.** Restart clones the wiring-time actor
    value: restart = state reset to initial. Per-incarnation resources
    (connections, handles) are acquired in `on_start` — the OTP `init` idiom —
    where failures are ordinary restartable failures.
  - **Mailboxes are bounded; cycles can deadlock.** Backpressure is affirmed
    (one graph-wide capacity knob; no per-actor override without evidence).
    The composition hazard with first-class cycles is documented contract:
    mutual `send` against two full mailboxes deadlocks permanently, and
    `call` cycles deadlock at depth 1. Idioms: `try_send` on feedback edges;
    `call` only "downhill" along a DAG ordering.
  - **Start and shutdown are concurrent, not OTP-sequential.** Children spawn
    in declaration order but initialize concurrently with no readiness
    gating; dependency ordering self-resolves because sends wait on bindings
    (P2). Shutdown cancels concurrently under one shared max-grace deadline
    (never compounding per-child graces). Corollary: drain loops must
    tolerate `SendError` from already-stopped siblings.

- **D11 (2026-07-09) — `Graph` and `ActorSet` merge into one type.** With
  D4, `Graph` was a spec you could only decompose once and `ActorSet` was
  that spec after decomposition — a two-stage pipeline whose guard only
  policed the seam between the stages. Now `GraphBuilder::build()` returns
  the runnable set directly (name kept: `Graph`, with `.actors()` in
  declaration order). Deleted: `ActorSet`, `into_actor_set`, the decompose
  guard, and the entire `GraphError` enum — `ActorRunError::AlreadyRunning`
  (per-actor, one run at a time) is the only run guard the system needs.
  `SupervisedActors::new` takes the merged `Graph` directly.

- **D12 (2026-07-09) — `tokio-actor` merges into `tokio-otp`.** The workspace
  becomes four crates: `tokio-supervisor` (standalone, genuinely
  independent), `tokio-otp` (actor layer + runtime layer, one crate),
  `tokio-otp-derive` (renamed from `tokio-actor-derive`; generated code now
  references `::tokio_otp` paths), and `tokio-otp-console`. Rationale: the
  actor↔runtime seam's failure mode is a silent sender hang untestable from
  either side alone. In the merged crate the seam becomes internal: the
  `Restart` → `RebindPolicy` mapping is a private function, the
  `terminate_binding` drop-guard obligation is a crate-internal invariant
  owned by one test suite, and the duplicated one-run-at-a-time enforcement
  can collapse. `RebindPolicy` and `run_until(shutdown, rebind)` remain
  public for hand-driving, but no external caller has to reconstruct the
  mapping to use the system correctly. Features consolidate on `tokio-otp`:
  `derive` (default), `console`, `metrics` (forwards to
  `tokio-supervisor/metrics`). Preludes reduce to two (supervisor; otp,
  which re-exports it) with one mirror test. Accepted costs: no standalone
  actor crate (hand-driving pulls the small supervisor dependency), and no
  independent versioning of the actor layer.

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

---

## 2. `tokio-supervisor`

### 2.1 Construction

- `ChildSpec::new(id, factory)` pairs a child id with an async factory
  `Fn(ChildContext) -> Future<Output = Result<(), BoxError>>`. The factory is
  re-invoked on every (re)start. Builder-style setters: `.restart(Restart)`,
  `.shutdown(ShutdownPolicy)`, `.restart_intensity(RestartIntensity)`
  (per-child override). ChildSpecs are cheaply cloneable and reusable.
- `SupervisorBuilder` collects children plus `strategy`, default
  `restart_intensity`, `control_channel_capacity` (default 64),
  `event_channel_capacity` (default 256).
- `build()` validates: non-empty, unique child ids; non-zero capacities;
  valid intensity/backoff (non-zero durations, non-zero factor). Errors:
  `SupervisorBuildError`. Zero children is valid — the supervisor starts
  idle (D1).
- A built `Supervisor` is cloneable; each clone runs an independent tree
  sharing the same (ref-counted) child specs.

### 2.2 Policies

- `Restart`: `Permanent` (always restart), `Transient` (default; restart on
  `Err`/panic/abort, not on `Ok`), `Temporary` (never restart).
- `Strategy`: `OneForOne` (default; only the failed child restarts),
  `OneForAll` (every unexpected exit stops and restarts the whole group;
  `Temporary` children are drained with the group but not respawned; the old
  generation is fully drained before the new one spawns — no overlap).
- `RestartIntensity { max_restarts, within, backoff }` (default 5 in 30s, no
  backoff): sliding window of restart timestamps; breach ⇒ supervisor exits
  with `SupervisorError::RestartIntensityExceeded`. Per-child overrides track
  their own window.
- `BackoffPolicy`: `None` | `Fixed(d)` | `Exponential{base,factor,max}` |
  `JitteredExponential` (equal jitter into `[d/2, d]`). The exponential
  attempt count is a per-child consecutive-restart counter, reset once a run
  survives longer than the intensity window (D8/L14) — independent of the
  intensity window's timestamp eviction.
- `ShutdownPolicy { grace, mode }`, default `CooperativeThenAbort` 5s.
  `Cooperative` = cancel, wait `grace`, then abort *and report timeout error*;
  `CooperativeThenAbort` = same but timeout is not an error; `Abort` = abort
  immediately. When draining several cooperative children at once (shutdown,
  group restart), one shared deadline = max grace among them (grace periods do
  not compound serially).

### 2.3 Running and lifecycle

- `Supervisor::spawn() -> SupervisorHandle` (background task) is the only way
  to run a supervisor (D8/L15); foreground driving is `spawn()` +
  `handle.wait()`.
- Children spawn in insertion order but initialize concurrently — readiness
  is not gated (D10). Each receives a fresh
  `ChildContext { id, generation, shutdown_token, supervisor_token }`.
  Generation starts at 0 and increments per restart. `SupervisorToken` is a
  read-only view of supervisor-wide cancellation.
- Exit classification: `Ok` ⇒ Completed; `Err` ⇒ Failed; panic ⇒ Panicked;
  task aborted ⇒ Aborted. All but Completed count as failure for restart
  policy.
- No natural exit (D1): the supervisor is a service. Once every child has
  finished (completed or terminally failed without restart) it idles at zero
  running children, keeps serving control commands, and continues until
  explicit shutdown or an intensity escalation. Per-child outcomes remain
  visible as `ChildSnapshot::last_exit`; run-to-completion is a caller
  pattern (`wait_for_snapshot` + `shutdown()`), not an engine mode.
- Explicit shutdown: `handle.shutdown()` is non-blocking and idempotent;
  cancellation wins over any pending restart delay, including zero-delay
  restarts. If cooperative-mode children time out during shutdown, `wait()`
  returns `Err(SupervisorError::ShutdownTimedOut(ids))`.
- `handle.wait()` resolves `Ok(())` after a graceful shutdown, or
  `Err(SupervisorError)` on intensity escalation / shutdown timeout (D1: no
  `SupervisorExit` enum). Safe to call from any number of clones concurrently
  (first caller joins the task, the rest share the result). A successful
  return means all child tasks are joined. Dropping the last handle clone
  requests graceful shutdown, exactly as `shutdown()` would (D2).

### 2.4 Dynamic children

- `add_child` / `remove_child` await control-channel capacity (D8: no `try_`
  variants, no `ControlError::Busy`). Adds validate like `build()` and start
  the child immediately.
- `remove_child` stops the child per its shutdown policy before resolving;
  concurrent duplicate removals error (`ChildRemovalInProgress`). Removing
  the last child is valid; the supervisor idles at zero children (D1).
- There is no path addressing (D7): to mutate a nested supervisor, obtain its
  stable handle (§2.5) and call `add_child`/`remove_child` on *it*. Handles
  are `Clone + Send` scoped capabilities — capture one in actor state or
  deliver it by message to control the tree from inside an actor.

### 2.5 Nested supervisors (D7)

- Nesting is a first-class child kind: `SupervisorBuilder::supervisor(id,
  sup)` statically, `SupervisorHandle::add_supervisor(id, sup)` dynamically;
  restart/shutdown/intensity policies attach as for any child. Contract:
  - parent cancellation (or abort of the nested child) ⇒ graceful shutdown of
    the nested tree (D2);
  - nested exit maps to child result: graceful shutdown ⇒ `Ok`, escalation or
    internal error ⇒ `Err` (so the parent's restart policy applies). Under D1
    a nested subtree never exits on its own, so batch-style subtrees no
    longer bubble completion to the parent — a future `run_to_completion`
    helper would restore that;
  - nested lifecycle events surface in the parent's event stream wrapped in
    `SupervisorEvent::Nested { id, generation, event }` (recursively;
    `.path()` / `.leaf()` unwrap them); forwarding is best-effort — a lagging
    forwarder drops events with a warning + metric;
  - the nested tree's snapshot appears (coalesced, eventually consistent)
    under the parent's `ChildSnapshot::supervisor` field.
- Mechanism (one, explicit): the parent passes a `ParentLink` (event sink,
  snapshot cell, id+generation) into each incarnation of the nested
  supervisor. No task-locals; a supervisor child behaves identically no
  matter what spawns it.
- Restart-stable nested handles: the parent owns per-nested-child stable
  channels (event broadcast, snapshot watch, control slot) that each
  incarnation binds into. `SupervisorHandle::supervisor(id)` returns the
  stable handle of a direct nested supervisor — keep it, pass it around;
  subscriptions and snapshots on it survive nested restarts. Between
  incarnations, control operations fail fast (`ControlError::Unavailable`-
  class) rather than queueing.

### 2.6 Observability

- `handle.subscribe()` → bounded broadcast of `SupervisorEvent`, 10 variants
  (D8/L11): Started, Stopping, Stopped, Nested, ChildStarted, ChildRemoved,
  ChildExited{status}, ChildRestartScheduled{delay} (also emitted for the
  triggering child of a `OneForAll` group restart), ChildRestarted{old,new},
  RestartIntensityExceeded. Slow subscribers observe `Lagged`.
  Removal-in-progress is snapshot state (`membership: Removing`), not an
  event.
- `handle.snapshot()` / `subscribe_snapshots()` → `watch` of
  `SupervisorSnapshot { state, strategy, children }` (D1: the supervisor-level
  `last_exit: Option<SupervisorExit>` field is dropped along with the enum;
  `state == Stopped` plus per-child `last_exit` carry the same information);
  `ChildSnapshot { id, generation, state (Starting|Running|Stopping|Stopped),
  membership (Active|Removing), last_exit, restart_count, next_restart_in,
  supervisor }`. Lookup helpers `child(id)` / `descendant(path)`.
- **Ordering guarantee (P3):** snapshot updated before event broadcast.
- `handle.event_sender()` exposes the broadcast sender (console needs one
  receiver per WebSocket).
- `handle.monitor_restart(id)` → `RestartMonitor`, an IntoFuture that captures
  the child's current generation synchronously and resolves with the new
  generation once the child is observed Running with a greater one; errors if
  the child is removed or the supervisor stops first. Direct children only.
- Prelude extension traits: `wait_for_event(predicate)` on the event receiver,
  `wait_for_snapshot(predicate)` on the snapshot receiver. (D8/L9 deleted
  `wait_until_running`; the all-running predicate over the snapshot is the
  documented replacement.)
- `tracing`: `supervisor` and `child` info spans (name, path, child_id,
  generation fields); every event also logged with a `root.`-prefixed
  dot-path. `metrics` (feature): children.running gauge, started/exited/
  restarts/intensity-exceeded/events.dropped/shutdown_timeouts counters,
  child_shutdown.duration histogram — all labeled by supervisor/path/strategy.

---

## 3. Actor layer of `tokio-otp` (D12; formerly the `tokio-actor` crate)

### 3.1 Actor model

Naming per D3: `Actor` is the handler-style trait (formerly `MessageHandler`);
`RawActor` is the custom-loop trait (formerly `Actor`).

- `RawActor` trait (the escape hatch): `type Msg: Send + 'static` +
  `async fn run(&self, ctx: ActorContext<Msg>) -> ActorResult` — full control
  of the receive loop. Actor values are `Clone + Send + Sync + 'static`;
  cloned per run. Not implementable by bare closures (deliberate). All
  registration surfaces (`GraphBuilder`, Topology fields, dynamic actors)
  accept any `RawActor`.
- `Actor` trait (the recommended surface): `async fn handle(&mut self, msg,
  &ctx)`, optional `on_start` / `on_stop` hooks, and `drain_policy()`. A
  blanket `impl<A: Actor> RawActor for A` provides the receive loop: on_start
  once, then messages in mailbox order, then on_stop (skipped if
  handle/on_start errored). `DrainPolicy::Discard` (default) stops at shutdown
  dropping queued messages; `Drain` closes the mailbox and handles everything
  already queued before stopping. Drains are not separately time-bounded — the
  surrounding shutdown timeout is the backstop — and must tolerate
  `SendError` from already-stopped siblings (D10: shutdown is concurrent).
- Restart = state reset (D10): each incarnation clones the wiring-time actor
  value; `Clone` is the reset mechanism. Per-incarnation resources are
  acquired in `on_start` (the OTP `init` idiom), where failure is an ordinary
  restartable failure.
- No internal supervision or execution (D4): the crate defines actors and
  their wiring; it does not run graphs. Each actor executes as one incarnation
  at a time via `RunnableActor::run_until` (§3.5), normally hosted as a
  supervisor child. What an exit means — restart, final completion,
  escalation — is entirely the host's policy, never the actor crate's.

### 3.2 Graph construction

- `GraphBuilder`: `add(actor)` (id = unqualified type name, `-2`/`-3` dedup),
  `actor(id, actor)` (explicit id), `slot::<M>(id) -> (ActorSlot<M>,
  ActorRef<M>)` + `define(slot, actor)` for cyclic wiring (refs are minted
  before actor values exist; the slot token is non-Clone so double-fill is
  unrepresentable; message-type mismatch is a compile error; cross-builder
  tokens are a build error). Config: `name` (default: generated
  `graph-N`), `mailbox_capacity` (default 64), `actor_shutdown_timeout`
  (default 5s). (D5 removed the blocking-task limit and
  `blocking_shutdown_timeout` knobs.)
- `build()` validates: ≥1 actor, unique non-empty labels, all slots filled,
  non-zero mailbox capacity, non-empty name. First error wins. Returns the
  runnable `Graph` directly (D11 — no separate decomposition step).
- Actor ids are **labels, not addresses** (D9): derive field names, type
  names, or explicit strings, consumed by tracing/stats/snapshots/console
  only. No API accepts a label as an address; addressing is typed refs
  minted at build time.
- `#[derive(Topology)]` (via `tokio-otp-derive` per D12, re-exported under
  the default `derive` feature): a named-field struct where each field is an actor
  generates `{Name}Refs` (one `ActorRef` per field) and
  `Name::graph(wire)` / `Name::graph_with(builder, wire)`; the `wire` closure
  receives all refs before any actor is constructed, so cycles need no
  forward declarations. Field names become actor labels. Rejects enums,
  tuple/unit structs, generics, zero fields; non-actor fields fail with E0277
  spanned at the field type. Visibility is inherited.

### 3.3 Execution model (D4, D11)

- A `Graph` is the wiring *and* the runnable set (D11): named actors, typed
  bindings, execution settings (mailbox capacity, shutdown timeout), and
  `.actors()` — the `RunnableActor`s in declaration order. There is no
  whole-graph run mode and no decompose step; the only run guard is
  per-actor (`ActorRunError::AlreadyRunning`).
- Execution = driving `RunnableActor::run_until(shutdown)` per actor —
  normally as supervisor children via `tokio-otp`, or by hand in tests. Refs
  are usable as soon as the graph is built; sends wait for the target's
  mailbox to bind.
- Per-run shutdown sequence: cancel the actor's token, wait up to
  `actor_shutdown_timeout`, abort the straggler. An abort during a
  *requested* shutdown is reported as clean; otherwise it is a failure.
- Fate-sharing across actors is expressed with supervisor strategy
  (`OneForAll`) or tree shape — never as an execution mode.

### 3.4 ActorRef, mailboxes, Reply (P2)

- `ActorRef<M>` (cloneable, `id()`, `stats()` per D6): bound to the actor's
  long-lived binding slot, in one of three states — Unbound (waiting for a
  run), Bound (live mailbox), Terminated (no future run expected).
- Delivery contract (D10): mailboxes are **incarnation-owned and bounded**;
  delivery is **at-most-once** with loss windows at restart and shutdown.
  Stronger guarantees are user protocol built on `call`/`Reply`.
- `send(msg).await`: waits for a binding, waits for capacity, and rides
  through restart windows *when a rebind is expected*; errors with
  `SendError::ActorTerminated` only when no restart is coming (or the binding
  source is gone). Messages accepted by a mailbox that dies before processing
  are lost — restarts bind a *fresh* mailbox; queued messages behind a poison
  message are dropped. Cancelling a pending send drops the message.
- `try_send(msg)`: non-blocking; `MailboxFull` / `MailboxClosed` /
  `ActorNotRunning` / `ActorTerminated`.
- Cycle hazard (D10, documented contract): mutual `send` against two full
  mailboxes deadlocks permanently; `call` cycles deadlock at depth 1. Idioms:
  `try_send` on feedback edges; `call` only "downhill" along a DAG ordering.
- `call(|reply| Msg::…) .await`: request/response via a one-shot `Reply<T>`
  carried in the message; actor answers with `reply.send(v)`; a dropped reply
  ⇒ `CallError::ReplyDropped`.
- `ActorContext<M>`: `id()`, `recv()` (**fail-fast at shutdown**: returns
  `None` as soon as shutdown is requested even with queued messages),
  `try_recv()` (bypasses shutdown check, for hand-written drain loops),
  `myself()`, `shutdown_token()`, `is_shutting_down()`, and `run_blocking`
  (§3.6). (D9 deleted `registry()`.)
- Whether a run's end leaves the binding Unbound (rebind expected) or
  Terminated is governed by `RebindPolicy` (`Always` / `OnFailure` / `Never`),
  passed as an argument to each `run_until` call (D8/L6 — no mutable
  pre-set state, so a stale policy is unrepresentable). The
  `Restart` → `RebindPolicy` mapping (`Permanent→Always`,
  `Transient→OnFailure`, `Temporary→Never`) and the drop guard that calls
  `terminate_binding` when no further run will come are crate-internal to the
  merged `tokio-otp` (D12) — a private invariant owned by one test suite, no
  longer a cross-crate convention. Hand-drivers using the public `run_until`
  own the same obligation, documented on the method.

### 3.5 Runnable actors — the execution surface

- `graph.actors()` (D11) exposes the `RunnableActor`s, which share the
  graph's binding slots so refs keep working across independent restarts.
- `RunnableActor`: `label()`,
  `run_until(shutdown, rebind: RebindPolicy).await` (D8/L6; one incarnation:
  fresh mailbox, binding bound for the duration, §3.3 timeout/abort
  semantics; at most one concurrent run — `ActorRunError::AlreadyRunning`;
  panics resume unwinding so a supervisor observes a panic), and
  `terminate_binding` (fail senders fast when no further run is coming).
  (D9 deleted `set_registry`/`register_with` and the string/typed lookup
  `actor_ref` methods.)
- `graph.dynamic_factory()` → `RunnableActorFactory`: mints new runnable
  actors at runtime sharing the graph's execution settings (also
  constructible standalone with defaults). `factory.actor(label, actor)`
  returns `(RunnableActor, ActorRef<A::Msg>)` — the ref is typed at creation
  (D9); no lookup, no downcast.
- Name-based discovery, where an application wants it, is the documented
  `Directory<M>` userland pattern: an ordinary actor holding
  `HashMap<String, ActorRef<M>>` (D9).

### 3.6 Blocking work (D5)

- One method, no types:
  `ctx.run_blocking(f: impl FnOnce(&CancellationToken) -> R + Send + 'static)
  -> R` — runs `f` on the tokio blocking pool and waits for it. The token is
  a child of the actor's shutdown token and is also cancelled if the
  `run_blocking` future is dropped; long-running closures should check
  `token.is_cancelled()` periodically. A panic in `f` resumes on the actor
  (an ordinary actor failure under supervision). The closure's return value
  is opaque to the framework — blocking failures are whatever `R` encodes,
  and the actor decides what they mean.
- Shutdown backstop: the actor awaits `run_blocking`, so cancellation is
  cooperative and the actor's own `actor_shutdown_timeout` bounds a closure
  that ignores the token (the blocking thread then runs detached, as tokio
  blocking tasks cannot be aborted).
- Detached / concurrent blocking work is a documented pattern, not API:
  clone `ctx.myself()`, call `tokio::task::spawn_blocking` directly, and
  `try_send` the outcome back as a message. The mailbox is the completion
  mechanism; bring your own semaphore if you need admission control.

### 3.7 Observability & inspection (D6)

- **Pull-based stats**: every actor exposes `ActorStats` —
  `messages_received`, `messages_accepted`, `sends_rejected` (relaxed atomics
  on the binding core; one increment per message/outcome) — plus live
  `mailbox_depth` / `mailbox_capacity` computed on read. Available via
  `ActorRef::stats()` and enumerable through `graph.actors()` and
  `RuntimeHandle`; the console renders them next to the supervision tree.
  Incarnation data (generation, restart_count, last_exit) remains in the
  supervisor snapshot.
- **Tracing**: `actor` spans, lifecycle logs for actor and mailbox events,
  per-message `trace!` events (level-filterable, free when disabled).
- **No metrics backend in the actor layer** (D6): exporting time-series is a
  user-side sampler task over the stats/snapshot surfaces (documented
  pattern/example). `tokio-otp/metrics` merely forwards to the supervisor's
  lifecycle metrics (D12).

---

## 4. Runtime layer of `tokio-otp`

### 4.1 Composition modes

1. **Integrated runtime** (flagship): `Runtime::builder().graph(g)
   .strategy(…).restart(…).shutdown(…).restart_intensity(…).build()` — every
   graph actor becomes its own supervised child with uniform policies.
   `.graph()` is optional (D1/D9): a graph-less runtime starts empty and
   grows via `add_actor`; there is no `.dynamic()` flag and no
   `MissingGraph` error.
2. **Per-actor supervision, à la carte**: `SupervisedActors::new(graph)` with
   defaults + per-actor overrides keyed by ref (D9:
   `actor_restart(&refs.x, …)`, `actor_shutdown`, `actor_restart_intensity` —
   typo-impossible, no `UnknownActor` error) →
   `build()` (Vec<ChildSpec> for embedding in any supervisor tree) /
   `build_supervisor(SupervisorBuilder)` / `build_runtime(SupervisorBuilder)`.
3. ~~Whole-graph supervision~~ — removed (D4). `SupervisedGraph` is deleted;
   "all actors restart together" is expressed as `SupervisedActors` +
   `Strategy::OneForAll` (or a nested subtree for scoped blast radius), which
   gives the same fate-sharing with per-actor observability.

### 4.2 Runtime & handle

- `Runtime::spawn() -> RuntimeHandle`; `into_supervisor()` is the escape
  hatch to the raw `Supervisor` (loses `add_actor` support — the actor
  factory stays behind).
- `RuntimeHandle` = full `SupervisorHandle` delegation (shutdown, wait,
  add/remove child (D7: no `_at` variants — use `supervisor(id)` handles;
  no `try_` variants per D8), `add_supervisor`, subscribe, snapshot(s),
  monitor_restart) plus:
  - `add_actor(label, actor, DynamicActorOptions{restart, shutdown,
    restart_intensity})` → mints the runnable actor from the runtime's
    factory, adds a supervised child (child id = label), returns the typed
    `ActorRef` (D9: no registry, no rollback bookkeeping; errors are
    `ControlError`). Removal is plain `remove_child(label)` — the child-spec
    drop guard terminates the binding.
  - `console()` (feature `console`) → pre-wired `ConsoleBuilder`.
- Actor children glue (the crate's real job): each `RunnableActor` becomes a
  `ChildSpec` whose factory runs `run_until(child cancellation)`; rebind
  policy derived from restart policy; a drop guard terminates the binding when
  the supervisor discards the child spec (intensity exhausted / removed /
  supervisor exit) so senders fail fast instead of waiting forever.
- Prelude: re-exports the supervisor prelude plus the actor-layer and
  runtime types (D12); one mirror test guards drift against
  `tokio_supervisor::prelude`.

---

## 5. `tokio-otp-console` (experimental)

Axum server with an embedded single-file HTML/JS frontend. Builder takes a
snapshot `watch::Receiver` and an event `broadcast::Sender` (one subscription
per WebSocket connection), binds `127.0.0.1:9100` by default; `spawn()` →
handle with `local_addr`/`shutdown`, or `run()`. Renders the supervision tree,
child states, events, and summary stats live; with D6, per-actor stats
(message counts, mailbox depth) join the tree view. Requires
`tokio-supervisor/serde`.

---

## 6. Feature flags and conventions

Workspace (D12): `tokio-supervisor`, `tokio-otp` (actor + runtime layers),
`tokio-otp-derive`, `tokio-otp-console`.

| Crate | Feature | Effect |
|---|---|---|
| tokio-supervisor | `metrics` | lifecycle metrics instruments (D6: the actor layer has none) |
| tokio-supervisor | `serde` | Serialize/Deserialize on snapshot/event views |
| tokio-otp | `derive` (default) | re-export `#[derive(Topology)]` |
| tokio-otp | `metrics` | forwards to `tokio-supervisor/metrics` (D6) |
| tokio-otp | `console` | re-export console + `RuntimeHandle::console()` |

Cross-crate identities: `BoxError` is the same type in both crates;
supervisor `ChildResult` and actor `ActorResult` are both
`Result<(), BoxError>`. `tokio-supervisor` is independently usable;
`tokio-otp` builds on it (D12) and is the product. Two preludes (supervisor;
otp, which re-exports it), one mirror test.

---

## 7. Complexity ledger

Legend — **Keep**: essential, cost justified. **Shrink**: keep the capability,
reduce surface/mechanism. **Drop**: candidate for removal. Evidence counts are
files referencing the feature (examples+book / integration tests).

| # | Feature | Public surface | Machinery it alone requires | Evidence | Recommendation |
|---|---|---|---|---|---|
| L1 | Blocking-work subsystem (`spawn_blocking` + handle/reap/escalation) | 9 types + 2 ctx methods + 3 builder knobs (≈¼ of the actor prelude) | `blocking.rs` (~660 loc): admission semaphore, claim-for-wait protocol, unbounded completion channel, reaper select-arm inside *every* actor run, per-task spans/metrics | 3+4 / 3 | **✅ DECIDED (D5)**: replaced by a single zero-type `ctx.run_blocking(FnOnce(&CancellationToken) -> R)`; detached work = documented `spawn_blocking` + result-as-message pattern. `blocking_work.rs` ports; `blocking_lifecycle.rs` becomes the pattern example; `blocking_limits.rs` dies with the limit. |
| L2 | Per-message metrics & send timing | none (invisible) | `Arc<OnceLock<GraphObservability>>` threaded through every binding/ref/slot/registry entry; timing + emit on every send/try_send/recv | dashboards only | **✅ DECIDED (D6)**: replaced by pull-based `ActorStats` (atomics on the binding) + live mailbox depth, readable via refs/set/registry/console; `tokio-actor` loses its `metrics` feature and the `OnceLock` plumbing; exporting = user-side sampler pattern. |
| L3 | Path-addressed nested control plane (`add_child_at`/`remove_child_at` + `try_` forms) | 8 handle methods ×2 (SupervisorHandle + RuntimeHandle) | global path-keyed `NestedControlRegistry`, task-local control scope, registration drop-guards | 2+1 / 1 | **✅ DECIDED (D7)**: deleted. Nested control = hold the nested supervisor's restart-stable handle (`SupervisorHandle::supervisor(id)`); handles are scoped capabilities capturable inside actors. |
| L4 | Nested event + snapshot forwarding | `SupervisorEvent::Nested`, `EventPathSegment`, `ChildSnapshot::supervisor`, `path()`/`leaf()`/`descendant()` | task-local event re-wrapper; coalescing snapshot bridge (atomic queue flag, overflow re-spawn); per-child nested-snapshot state | nested example, book, console tree view / 2 | **✅ DECIDED (D7)**: contracts kept (Nested events, recursive snapshots, P3 ordering); mechanism replaced — nesting is a first-class child kind receiving an explicit `ParentLink`, task-locals deleted, `into_child_spec` replaced by `SupervisorBuilder::supervisor` / `add_supervisor`. Bonus: nested subscriptions survive restarts. |
| L5 | `try_add_child` / `try_remove_child` (+ Busy error) | 4 methods ×2 handles | try_send paths through control endpoint | 0 / 1 | **✅ DECIDED (D8)**: deleted; control ops await channel capacity. |
| L6 | `RebindPolicy` / binding lifecycle exposure (`set_rebind_policy`, `terminate_binding`, `RunDisposition` matrix) | 1 enum + 2 methods, but a subtle 3-state contract | disposition matrix in actor_set; drop-guard in tokio-otp; policy mapping table | internal to tokio-otp / 2 | **✅ DECIDED (D8)**: `run_until(shutdown, rebind)` takes the policy per run; `set_rebind_policy` deleted; `terminate_binding` remains as the host's documented give-up obligation. |
| L7 | `ActorSet` / `RunnableActor` / `RunnableActorFactory` | 4 types | per-actor run guards | load-bearing for tokio-otp | **✅ DECIDED (D4)**: promoted from "integration bridge" to *the* execution surface — `into_actor_set` is a graph's only consumption. Graph tri-state reduces to a one-shot decompose guard. |
| L8 | Whole-graph run modes: `Graph::run_until` / `Graph::spawn` / `GraphHandle` / graph drive loop / `SupervisedGraph` | 4 methods + 2 types + 4 `GraphError` variants | ~400-loc drive loop, tri-state, duplicated deadline/abort logic | 3+? / 2 | **✅ DECIDED (D4)**: deleted entirely. `RunnableActor::run_until` is the sole run primitive; fate-sharing = `OneForAll` supervision. Affected examples/doctests port to `Runtime` or hand-driven `run_until`. |
| L9 | `wait_until_running` | 1 method ×2 handles | recursive snapshot predicate | 0 / 1 | **✅ DECIDED (D8)**: deleted; `wait_for_snapshot(all-running predicate)` is the documented replacement. |
| L10 | `monitor_restart` / `RestartMonitor` | 2 types, 1 method ×2 handles | generation-baseline observer over snapshots | 4 / 2 | **Keep**: small, contractually crisp (synchronous baseline capture), used by book & examples; it is the sanctioned way to await a restart. |
| L11 | `SupervisorEvent` breadth (12 variants incl. RemoveRequested, RestartScheduled, GroupRestartScheduled) | 12-variant enum | per-variant emit + snapshot-update classification | console/examples render-only for the cut pair | **✅ DECIDED (D8)**: `ChildRemoveRequested` and `GroupRestartScheduled` deleted (verified: consumers display-only); 10 variants remain. |
| L12 | `allow_empty` + natural-completion exit machinery | 1 builder flag, `SupervisorExit`, 2 error variants | natural-exit branch, `TerminalStatus` bookkeeping, idle-shutdown special case, last-child-removal rules | dynamic runtime / ~8 run-to-completion tests | **✅ DECIDED (D1)**: always allow empty; drop natural exit, `SupervisorExit`, `EmptyChildren`, `LastChildRemovalUnsupported`, and all terminal-status tracking. `RuntimeBuilder::dynamic` reduces to "graph optional". Optional `run_to_completion()` helper later if wanted. |
| L13 | Per-child `RestartIntensity` overrides | 1 ChildSpec method | per-child tracker + validation paths | 4+1 / 1 | **Keep**: cheap (tracker exists per child anyway) and genuinely useful. |
| L14 | `BackoffPolicy::JitteredExponential` + hand-rolled xorshift RNG | 1 variant | JitterRng, seeding, nanos math | 0 / bench only | **✅ DECIDED (D8)**: variant kept; attempt count redesigned as a per-child consecutive-restart counter reset when a run outlives the intensity window. |
| L15 | Handle-less `Supervisor::run` / `Runtime::into_supervisor` | 2 methods | closed-control-channel special start path | 1 / 1 | **✅ DECIDED (D8)**: `Supervisor::run` deleted (foreground = `spawn()` + `wait()`, drop-to-teardown preserved by D2); `Runtime::into_supervisor` kept for nesting via `add_supervisor`. |
| L16 | Three mirrored preludes + drift-guard tests | 3 modules | mirror tests | universal | **Keep**: cheap and it's what makes `use tokio_otp::prelude::*` the one-import story. |
| L17 | Console crate | 3 types | axum/ws server, embedded frontend | experimental | **Keep as experimental**: isolated behind a feature; its main upstream costs are `serde` derives and `event_sender()` — both small. |
| L18 | `#[derive(Topology)]` | derive + generated Refs/graph fns | 255-loc proc macro | 2+10 / recently invested | **Keep**: recent, well-tested, and it is the ergonomic answer to cyclic wiring; `slot`/`define` stays as the dynamic fallback it compiles down to. |

### Themes for the refactor discussion

1. **The actor crate is two libraries.** A typed-mailbox actor core (traits,
   builder, refs) and a blocking-work sub-framework (L1). Resolved by D5: the
   sub-framework is replaced by one zero-type method, deleting ~600 loc and 9
   public types.
2. **Observability is the widest cross-cutting cost.** Resolved by D6: the
   push pipeline is replaced by pull-based stats on the binding, deleting the
   `OnceLock` slots and the per-message clock/backend work while *improving*
   inspectability (live mailbox depth, console-visible per-actor counters).
3. **Ambient context is the riskiest mechanism.** Resolved by D7: nesting is
   a first-class child kind receiving an explicit `ParentLink`; the three
   task-local channels and the path-keyed registry are deleted, and nested
   supervisors gain restart-stable handles (the `ActorRef` treatment).
4. **Integration seams should be narrower, not wider.** Resolved by D8/L6:
   the rebind policy is a per-run argument to `run_until`, so a stale or
   unset policy is unrepresentable; the one remaining cross-crate obligation
   (`terminate_binding` on give-up) is explicit and documented.
5. **Pure surface trims.** Resolved by D1/D7/D8: `allow_empty` +
   `SupervisorExit` + terminal-status machinery, the `_at` path family, the
   `try_` control variants, `wait_until_running`, two event variants, and
   handle-less `run` — ~25 public methods/variants/types removed without
   touching any pillar.

All ledger items are now resolved; the decision log (D1–D12) plus the spec
body constitute the target contract for the refactor.
