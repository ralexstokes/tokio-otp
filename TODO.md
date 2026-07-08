# Ergonomics TODO

High-level user-facing ergonomics improvements, stack-ranked by impact. From a
review of the public API surfaces, examples, the README, and the tutorial
book. Items near the top change what the items below them look like, so work
roughly in order.

## 1. ~~`ActorRef` send-method signatures and waiting semantics~~ — done

- ~~`send_when_ready` and `wait_for_binding` take `&mut self` (they wait on a
  `watch::Receiver`), so every actor storing a ref in `&self` state must
  clone per call — `let mut press = self.press.clone();` appears in the
  README's first code sample and the book. Clone the receiver internally so
  these take `&self`, matching `send`/`try_send`/`call`.~~
- ~~Decide whether `send_when_ready` should support a caller-provided
  cancellation token or timeout helper for cases where waiting forever is not
  desirable. The same question applies to `call`, which today has no deadline
  and fails immediately with `ActorNotRunning` during a restart window
  instead of waiting for the rebind.~~
- ~~`ActorRef::blocking_send` has try-send semantics, but tokio's
  `mpsc::Sender::blocking_send` blocks for capacity; the name imports the
  opposite expectation. Rename (e.g. `send_from_blocking`) while the API is
  unpublished.~~

Done by folding restart-aware waiting into `send`/`call`, deleting
`send_when_ready`, `wait_for_binding`, and `blocking_send`, and leaving
timeouts/cancellation to `tokio::time::timeout` / `select!`.

## 2. ~~First-class readiness and restart-waiting helpers~~ — done

~~Every example calls per-ref `wait_for_binding().await` after `spawn()`. Add
helpers like `RuntimeHandle::wait_until_running()` (all children started).~~
~~Waiting for a restart still means hand-matching
`ChildStarted { generation > 0 }` events — with a subtle race if you subscribe
after triggering the crash. Add `wait_for_child_restart(id)` to cover that
remaining pattern.~~

Done with `SupervisorHandle::monitor_restart` / `RuntimeHandle::monitor_restart`
returning `RestartMonitor`, plus `SupervisorHandle::wait_until_running`.

## 3. ~~Umbrella prelude completeness~~ — done

~~`tokio_supervisor::prelude` has the `wait_for_event` / `wait_for_snapshot`
extension traits, but `tokio_otp::prelude` does not re-export them — so
`examples/supervised_actors.rs` hand-rolls a nine-line event loop that
`wait_for_event` exists to eliminate. The umbrella prelude is also missing
`SendError`, `CallError`, `ChildResult`, `BackoffPolicy`, and the
`BlockingOptions` / `BlockingContext` types needed to call
`ctx.spawn_blocking`.~~

Done by defining `tokio_otp::prelude` as the union of the sub-crate preludes
minus `BuildError` and the duplicate `tokio_supervisor::BoxError`, enforced by
`crates/tokio-otp/tests/prelude.rs`.

## 4. Foreground `Runtime::run` loses the control surface

`Runtime::run()` consumes the runtime with no handle — no shutdown, no
events, and on a dynamic runtime the registry is silently discarded, so
dynamic-actor support evaporates depending on how you start it. Similarly
`Runtime::into_parts` returns only the `Supervisor`, dropping the registry,
and the name implies more than one part. Either offer `runtime.handle()`
before `run()` (like tokio's own `Runtime`), or document `spawn()` + `wait()`
as the only full-featured path and rename `into_parts` to `into_supervisor`.

## 5. `actor_shutdown_timeout` only applies in graph-run mode

`Graph::run_until` aborts uncooperative actors after the timeout, but
`RunnableActor::run_until` cancels the token and then waits indefinitely
(the timeout is never threaded into `RunnableActorInner`; under
`tokio-supervisor` the child `ShutdownPolicy` timeout is the backstop).
Either thread the timeout through `RunnableActor` or document the asymmetry
on both the builder setter and `RunnableActor::run_until`.

## 6. Registry consistency and lookup diagnostics

- ~~Registry entries go stale when a dynamic actor is removed outside
  `RuntimeHandle::remove_actor` (direct `remove_child`, or a transient actor
  exiting for good): `actor_ids()`/`contains()` keep advertising an actor
  whose ref only returns `ActorNotRunning`. Document or add event-driven
  cleanup.~~
- `RuntimeHandle::actor_ref` on a runtime built without a dynamic registry
  (`Runtime::new`) returns `LookupError::UnknownActor` for every id,
  conflating "no registry configured" with "no such actor" (`add_actor`
  distinguishes the same case as `DynamicActorError::Unsupported`).

## 7. Naming and diagnostics polish

- `ChildContext` exposes a public `ctx.token` field while `ActorContext` uses
  `ctx.shutdown_token()`; pick one convention across crates, and fold the
  `ChildContext::supervisor_token` / `SupervisorToken` naming into the same
  pass.
- Builder diagnostics: registering a duplicate id with a different message
  type reports `MessageTypeMismatch` rather than `DuplicateActorId`, and
  `build()` reports an empty graph name before any accumulated registration
  errors.
- Three distinct `BuildError` types (`tokio-actor`, `tokio-supervisor`,
  `tokio-otp`) and two identical `BoxError` aliases exist across the crates.
  Workable, but a naming/unification pass is cheap while unpublished.

## 8. Docs front-door polish

- The `tokio-otp` crate-level doc example is ` ```ignore ` and references an
  undefined `graph` variable — the one example on the front door is the only
  untested one (doctests run in CI everywhere else). Make it a compiling
  `no_run` example.
- The book's rustdoc-style links (e.g. `getting-started.md`) all point at the
  GitHub tree root as placeholders. Publish rustdoc alongside the mdBook and
  link to it.

## 9. Future actor ergonomics

- Named registry aliases, e.g. `builder.alias("orders", "front-desk")`, for
  friendlier external lookup names.
- Optional topology metadata for graph visualizers now that links are no
  longer part of the send API.
- Codec helpers for byte boundaries, such as serde-based decode/encode
  utilities at application edges.
- Optional message-size observability hooks for applications that can measure
  their typed messages meaningfully.

## 10. Future extensions from a real-world design exercise

From sketching a multi-venue trading system (per-venue market-data and
execution subtrees nested under a root runtime, strategy actors as the root
graph, per-position dynamic actors). The overall shape mapped well — nested
supervisors via `into_child_spec` for venue isolation, `Restart::Permanent` +
`RestartIntensity` as a free reconnection policy for WebSocket reader actors,
restart-stable refs across subtree restarts, `SupervisorEvent::Nested` for
single-subscription monitoring. Three gaps surfaced:

- **`rest_for_one` strategy.** A feed pipeline like `ws-reader →
  book-builder` is the textbook case: if the reader dies, restart it and
  everything downstream of it (the book state is invalid after a feed gap),
  but a downstream crash alone should not bounce a healthy upstream
  connection. With only `OneForOne`/`OneForAll` you either under-restart or
  over-restart — and `OneForAll` means dropping and re-handshaking a healthy
  socket, which is real latency for connection-oriented children.
- **Dynamic children under nested supervisors.** `add_actor`/`remove_actor`
  live on `RuntimeHandle`, so only the root runtime can host dynamic actors;
  nested subtrees built from `ChildSpec`s cannot. "Spawn a per-symbol
  subscription actor inside an already-running venue subtree" has no home
  today. The OTP analogue is `simple_one_for_one` / Elixir's
  `DynamicSupervisor`: a supervisor node whose children are added and removed
  at runtime, anywhere in the tree.
- **Conflating (latest-wins) mailbox option.** `send` backpressure
  is right for command-like messages but wrong for high-rate state updates
  (market data ticks): a slow consumer should never backpressure the
  producer into falling behind its source, and draining a deep queue of
  stale updates is worse than skipping to the newest. Today each producer
  hand-rolls conflation; a per-actor mailbox mode that keeps only the latest
  message (or latest per key) would cover it declaratively.

A broader survey of OTP features surfaced more gaps, ranked by impact. For
scoping: much of `gen_server` is already covered — `call` with a `Reply<T>`
value matches `GenServer.call` including deferred replies (stash the `Reply`,
answer later, i.e. `GenServer.reply/2`), `on_start`/`on_stop` map to
`init`/`terminate`, restarts have `BackoffPolicy`, and shutdown walks
children in reverse start order. Distribution, hot code upgrade, ETS, and
`gen_event` are deliberately out of scope (they fight the Rust/tokio grain
or are discouraged even in OTP).

- **Timers: `send_after` / periodic messages to self.** The most-used OTP
  pattern with no equivalent: heartbeats, reconnect delays, order timeouts,
  periodic reconciliation all schedule messages to self (`Process.send_after`,
  gen_server timeouts, `:timer.send_interval`). `ActorContext` has nothing,
  and it compounds: `MessageHandler` owns the receive loop (no hand-rolled
  `select!` over an interval without dropping to raw `Actor::run`), and the
  context exposes no ref to self (`registry()` is `None` on non-dynamic
  runtimes), so the workaround is a side task holding your own `ActorRef` —
  exactly the boilerplate the framework loop exists to eliminate. Sketch:
  `ctx.send_after(msg, duration)` / `ctx.interval(msg, period)`, with timers
  cancelled on restart.
- **Actor-to-actor monitors.** OTP's `Process.monitor` delivers a peer's
  death as a message in the observer's mailbox — the bread-and-butter
  primitive for "I depend on X for this request; tell me if it dies." Today
  death notification exists only as the supervisor-handle event stream
  (`subscribe()`), which lives outside the actor model: an actor cannot
  easily consume it and it requires plumbing the handle in. Sketch:
  `ctx.monitor("order-gateway")` injecting a typed
  `Down { id, generation }`-style message.
- **Significant children / `auto_shutdown` (OTP 24+).** "This supervisor's
  purpose is complete when child X exits cleanly — shut down the rest."
  Today a `Transient` child exiting `Ok` just leaves a hole and the subtree
  idles on. Matters for pipeline-shaped subtrees (source finished → tear
  down downstream stages) and batch/job-runner use; composes with nesting
  since the parent sees a clean child exit.
- **Ordered, readiness-gated startup.** OTP starts children strictly in
  declaration order, each child's `init` completing before the next starts,
  so "my sibling above me is ready" is a guarantee, not a race. Our
  supervisor spawns everything concurrently into a `JoinSet`
  (`runtime/supervision.rs`). Section 2's readiness helper treats the symptom;
  an opt-in sequential start mode (child counts as started once `on_start`
  returns) would remove the race class entirely. OTP's `handle_continue` is
  the companion feature: a hook for expensive post-init work that should not
  block the start sequence.
- **`gen_statem`-style state machines (follow-on, not a peer).** An enum
  field in the handler covers most of it in Rust — except state timeouts
  ("no fill within 500ms of entering `PendingFill` → transition to
  `Cancelling`"), which are painful precisely because timers are missing.
  If timers land, this becomes thin sugar; sequence it after them.
