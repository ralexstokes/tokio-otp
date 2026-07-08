# Ergonomics TODO

High-level user-facing ergonomics improvements, stack-ranked by impact. From a
review of the public API surfaces, examples, the README, and the tutorial
book. Items near the top change what the items below them look like, so work
roughly in order.

## 1. `ActorRef` send-method signatures and waiting semantics

- `send_when_ready` and `wait_for_binding` take `&mut self` (they wait on a
  `watch::Receiver`), so every actor storing a ref in `&self` state must
  clone per call — `let mut press = self.press.clone();` appears in the
  README's first code sample and the book. Clone the receiver internally so
  these take `&self`, matching `send`/`try_send`/`call`.
- Decide whether `send_when_ready` should support a caller-provided
  cancellation token or timeout helper for cases where waiting forever is not
  desirable. The same question applies to `call`, which today has no deadline
  and fails immediately with `ActorNotRunning` during a restart window
  instead of waiting for the rebind.
- `ActorRef::blocking_send` has try-send semantics, but tokio's
  `mpsc::Sender::blocking_send` blocks for capacity; the name imports the
  opposite expectation. Rename (e.g. `send_from_blocking`) while the API is
  unpublished.

These touch the same methods; do them as one pass.

## 2. First-class readiness and restart-waiting helpers

Every example calls per-ref `wait_for_binding().await` after `spawn()`, and
waiting for a restart means hand-matching `ChildStarted { generation > 0 }`
events — with a subtle race if you subscribe after triggering the crash. Add
helpers like `RuntimeHandle::wait_until_running()` (all children started) and
`wait_for_child_restart(id)` to cover the two patterns every consumer needs.

## 3. Umbrella prelude completeness

`tokio_supervisor::prelude` has the `wait_for_event` / `wait_for_snapshot`
extension traits, but `tokio_otp::prelude` does not re-export them — so
`examples/supervised_actors.rs` hand-rolls a nine-line event loop that
`wait_for_event` exists to eliminate. The umbrella prelude is also missing
`SendError`, `CallError`, `ChildResult`, `BackoffPolicy`, and the
`BlockingOptions` / `BlockingContext` types needed to call
`ctx.spawn_blocking`.

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

- Registry entries go stale when a dynamic actor is removed outside
  `RuntimeHandle::remove_actor` (direct `remove_child`, or a transient actor
  exiting for good): `actor_ids()`/`contains()` keep advertising an actor
  whose ref only returns `ActorNotRunning`. Document or add event-driven
  cleanup.
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
