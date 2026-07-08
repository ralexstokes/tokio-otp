# Ergonomics TODO

High-level user-facing ergonomics improvements, ranked by impact. From a
review of the public API surfaces, examples, and the tutorial book.

## 1. Quick-start ceremony

The `tokio-actor` quick start still needs a manual `CancellationToken`, a
`tokio::spawn` clone dance, `wait_for_binding()`, then two-layer
`task.await??`. Mirror the supervisor API with a convenience like
`graph.spawn() -> GraphHandle` and `shutdown_and_wait()`.

## 2. Make `tokio-otp` unambiguously the front door

Getting-started still tells users about multiple crates. Lead the book and
README with `tokio-otp` + prelude, present the sub-crates as à la carte, and
consider a single `Runtime::builder().graph(graph).strategy(...)` path for the
common supervised-actor setup.

## 3. Consistency / semantics polish

- `ChildContext` exposes a public `ctx.token` field while `ActorContext` uses
  `ctx.shutdown_token()`; pick one convention across crates.
- Decide whether `ActorRef::send_when_ready` should support a caller-provided
  cancellation token or timeout helper for cases where waiting forever is not
  desirable. The same question applies to `ActorRef::call`, which today has
  no deadline and fails immediately with `ActorNotRunning` during a restart
  window instead of waiting for the rebind.
- `actor_shutdown_timeout` only applies in graph-run mode. `Graph::run_until`
  aborts uncooperative actors after the timeout, but
  `RunnableActor::run_until` cancels the token and then waits indefinitely,
  so the builder docs over-promise for decomposed actors (under
  `tokio-supervisor` the child `ShutdownPolicy` timeout is the backstop).
  Either thread the timeout through `RunnableActor` or document the
  asymmetry.
- Shutdown drops queued messages with no drain path: `ctx.recv()` returns
  `None` as soon as shutdown is requested, even with messages queued, and
  the mailbox is not otherwise reachable. In-flight `call`s at shutdown
  become `ReplyDropped`. Document the fail-fast semantics or add a
  non-blocking `try_recv` for drain-then-exit actors.
- `ActorRef::blocking_send` has try-send semantics, but tokio's
  `mpsc::Sender::blocking_send` blocks for capacity; the name imports the
  opposite expectation. Consider renaming (e.g. `send_from_blocking`) while
  the API is unpublished.
- Registry entries go stale when a dynamic actor is removed outside
  `RuntimeHandle::remove_actor` (direct `remove_child`, or a transient actor
  exiting for good): `actor_ids()`/`contains()` keep advertising an actor
  whose ref only returns `ActorNotRunning`. Document or add event-driven
  cleanup.
- `RuntimeHandle::actor_ref` on a runtime built without a dynamic registry
  (`Runtime::new`) returns `LookupError::UnknownActor` for every id,
  conflating "no registry configured" with "no such actor" (`add_actor`
  distinguishes the same case as `DynamicActorError::Unsupported`).
- Builder diagnostics: registering a duplicate id with a different message
  type reports `MessageTypeMismatch` rather than `DuplicateActorId`, and
  `build()` reports an empty graph name before any accumulated registration
  errors.

## 4. Future actor ergonomics

- Named registry aliases, e.g. `builder.alias("orders", "front-desk")`, for
  friendlier external lookup names.
- Optional topology metadata for graph visualizers now that links are no
  longer part of the send API.
- Codec helpers for byte boundaries, such as serde-based decode/encode
  utilities at application edges.
- Optional message-size observability hooks for applications that can measure
  their typed messages meaningfully.

## 5. Housekeeping

- `ActorSet::from_graph` returns `Result` but is infallible; the lib.rs
  quick start discards `wait_for_binding()`'s `bool`.
