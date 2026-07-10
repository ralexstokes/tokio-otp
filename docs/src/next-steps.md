# Where to go next

## API documentation

The rustdocs are the reference companion to this tutorial — the crate-level
docs in particular are worth reading in full:

```sh
just doc   # cargo doc --workspace --no-deps --open
```

## Runnable examples

Every feature covered here (and a few that weren't) has a focused, runnable
example. Run any of them with:

```sh
cargo run -p <crate> --example <name>
```

### `tokio-supervisor`

| Example | Shows |
|---------|-------|
| `one_for_one_restart` | Basic restart behaviour. |
| `one_for_all_pipeline` | Interdependent children with `OneForAll`. |
| `nested_supervisor` | Supervision trees. |
| `dynamic_children` | Adding and removing children at runtime. |
| `per_child_restart_intensity` | Per-child intensity overrides. |
| `shutdown_with_cancellation_token` | Graceful shutdown driven by a signal. |
| `subscribe_to_events` | Reacting to lifecycle events. |
| `subscribe_to_snapshots` | Polling supervisor state. |
| `tracing` | Structured logging output. |
| `metrics` | Prometheus metrics (needs `--features metrics`). |

### `tokio-actor`

| Example | Shows |
|---------|-------|
| `ref_rebind` | Stable typed actor refs across graph reruns. |
| `send_vs_try_send` | Waiting `send` vs fail-fast `try_send` across a restart window. |
| `mailbox_backpressure` | Bounded mailbox back-pressure. |
| `blocking_work` | Awaiting cooperative blocking work with `run_blocking`. |
| `blocking_lifecycle` | Detached blocking work returning results as actor messages. |
| `graph_failures` | Error propagation from actor failures. |
| `builder_validation` | Build-time graph validation errors. |
| `actor_tracing` | Structured actor and message tracing. |

### `tokio-otp`

| Example | Shows |
|---------|-------|
| `supervised_actors` | Per-actor supervision with default policies. |
| `individual_actor_policies` | Per-actor restart/shutdown overrides. |
| `dynamic_actors` | Adding and removing actors at runtime. |
| `supervisor_snapshot_trace` | Observing runtime state in detail. |
| `actor_metrics` | Pull-based actor stats and user-owned export sampling. |
| `console` | The live web console (needs `--features console`). |

## Design notes

Things this tutorial glossed over that matter in production:

- **Messages are ordinary owned Rust values.** Use enums to model protocol
  variants and put `Reply<T>` in a variant when callers need a response.
- **Messages in a failed actor's mailbox are lost** when it restarts. If an
  order must survive a press jam, persist it outside the graph and re-inject
  it — the same discipline OTP asks of you.
- **Restart budgets are your circuit breakers.** Tune `RestartIntensity` so a
  persistent fault escalates to something (a parent supervisor, your process
  manager, an alert) instead of looping forever.
- **Blocking work needs cancellation checks.** Cooperative shutdown is only as
  graceful as your `token.is_cancelled()` checks are frequent.
