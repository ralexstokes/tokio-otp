# Ergonomics TODO

High-level user-facing ergonomics improvements, stack-ranked by impact. From a
review of the public API surfaces, examples, the README, and the tutorial
book. Items near the top change what the items below them look like, so work
roughly in order. Resolved items are pruned; see git history for what was
done and why.

## 1. Future actor ergonomics

- Optional topology metadata for graph visualizers now that
  `#[derive(Topology)]` provides a natural hook for recording static shape
  without making users duplicate their wiring.
- Codec helpers for byte boundaries, such as serde-based decode/encode
  utilities at application edges.
- Optional message-size observability hooks for applications that can measure
  their typed messages meaningfully.

## 2. Future extensions from a real-world design exercise

From sketching a multi-venue trading system (per-venue market-data and
execution subtrees nested under a root runtime, strategy actors as the root
graph, per-position dynamic actors). The overall shape mapped well — nested
supervisors as first-class children (`SupervisorBuilder::supervisor` /
`SupervisorHandle::add_supervisor`) for venue isolation, `Restart::Permanent`
+ `RestartIntensity` as a free reconnection policy for WebSocket reader
actors, restart-stable refs across subtree restarts, `SupervisorEvent::Nested`
for single-subscription monitoring. Gaps that surfaced:

- **`rest_for_one` strategy.** A feed pipeline like `ws-reader →
  book-builder` is the textbook case: if the reader dies, restart it and
  everything downstream of it (the book state is invalid after a feed gap),
  but a downstream crash alone should not bounce a healthy upstream
  connection. With only `OneForOne`/`OneForAll` you either under-restart or
  over-restart — and `OneForAll` means dropping and re-handshaking a healthy
  socket, which is real latency for connection-oriented children.
- **`add_actor` convenience on nested supervisors.** The capability gap is
  closed: any `SupervisorHandle` supports `add_child`/`remove_child` at
  runtime, and `graph.dynamic_factory()` mints typed runnable actors on the
  fly. But wiring the two together — "spawn a per-symbol subscription actor
  inside an already-running venue subtree" — means hand-writing a `ChildSpec`
  around `RunnableActor::run_until`, duplicating the rebind/terminate-binding
  glue that `RuntimeHandle::add_actor` bundles (`actor_child_spec` is
  `pub(crate)`). Either expose that glue or put `add_actor` on
  `SupervisorHandle` too.
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
or are discouraged even in OTP). Name-based discovery is settled as the
userland `Directory<M>` pattern (see `examples/directory.rs`), not a
framework feature.

- **Timers: `send_after` / periodic messages to self.** The most-used OTP
  pattern with no equivalent: heartbeats, reconnect delays, order timeouts,
  periodic reconciliation all schedule messages to self (`Process.send_after`,
  gen_server timeouts, `:timer.send_interval`). `ActorContext` has nothing,
  and `Actor` owns the receive loop, so there is no hand-rolled `select!`
  over an interval without dropping to `RawActor::run`. `ctx.myself()` makes
  the workaround a spawned side task ticking messages back at yourself —
  small, but still per-timer boilerplate the framework loop exists to
  eliminate, and the side task outlives restarts unless you manage it.
  Sketch: `ctx.send_after(msg, duration)` / `ctx.interval(msg, period)`,
  with timers cancelled on restart.
- **Actor-to-actor monitors.** OTP's `Process.monitor` delivers a peer's
  death as a message in the observer's mailbox — the bread-and-butter
  primitive for "I depend on X for this request; tell me if it dies." Today
  death notification exists only as the supervisor-handle event stream
  (`subscribe()`), which lives outside the actor model: an actor cannot
  easily consume it and it requires plumbing the handle in. Sketch:
  `ctx.monitor(&actor_ref)` injecting a typed
  `Down { generation }`-style message.
- **Significant children / `auto_shutdown` (OTP 24+).** "This supervisor's
  purpose is complete when child X exits cleanly — shut down the rest."
  Today a `Transient` child exiting `Ok` just leaves a hole and the subtree
  idles on. Matters for pipeline-shaped subtrees (source finished → tear
  down downstream stages) and batch/job-runner use; composes with nesting
  since the parent sees a clean child exit. Related: the SPEC-noted
  `run_to_completion` helper for batch-style subtrees, since a nested
  subtree never exits on its own.
- **Ordered, readiness-gated startup.** OTP starts children strictly in
  declaration order, each child's `init` completing before the next starts,
  so "my sibling above me is ready" is a guarantee, not a race. Our
  supervisor spawns everything concurrently into a `JoinSet`
  (`runtime/supervision.rs`), and nothing on the handle surface lets a
  caller wait for readiness. An opt-in sequential start mode (child counts
  as started once `on_start` returns) would remove the race class entirely.
  OTP's `handle_continue` is the companion feature: a hook for expensive
  post-init work that should not block the start sequence.
- **`gen_statem`-style state machines (follow-on, not a peer).** An enum
  field in the handler covers most of it in Rust — except state timeouts
  ("no fill within 500ms of entering `PendingFill` → transition to
  `Cancelling`"), which are painful precisely because timers are missing.
  If timers land, this becomes thin sugar; sequence it after them.
