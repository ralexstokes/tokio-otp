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
  desirable.

## 4. Future actor ergonomics

- Named registry aliases, e.g. `builder.alias("orders", "front-desk")`, for
  friendlier external lookup names.
- Optional topology metadata for graph visualizers now that links are no
  longer part of the send API.
- Codec helpers for byte boundaries, such as serde-based decode/encode
  utilities at application edges.
- Optional message-size observability hooks for applications that can measure
  their typed messages meaningfully.
