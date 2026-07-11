# Supervision fundamentals

Time to open the print shop. In this chapter we stay in `tokio-supervisor`
land and supervise two plain tasks: a `front-desk` that should run forever,
and a `press` that keeps jamming. Along the way we meet every knob a
[`ChildSpec`] has.

```rust,no_run
use std::time::Duration;

use tokio_supervisor::{
    BackoffPolicy, ChildSpec, RestartPolicy, RestartIntensity, ShutdownPolicy, Strategy,
    SupervisorBuilder,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A press that jams shortly after starting.
    let press = ChildSpec::new("press", |ctx| async move {
        println!("press starting (generation {})", ctx.generation());
        tokio::time::sleep(Duration::from_millis(200)).await;
        Err("paper jam".into())
    })
    .restart(RestartPolicy::OnFailure)
    .restart_intensity(
        RestartIntensity::new(3, Duration::from_secs(10))
            .with_backoff(BackoffPolicy::Fixed(Duration::from_millis(100))),
    )
    .shutdown(ShutdownPolicy::cooperative_then_abort(Duration::from_secs(1)));

    // A front desk that runs until asked to stop.
    let front_desk = ChildSpec::new("front-desk", |ctx| async move {
        ctx.shutdown_token().cancelled().await;
        Ok(())
    })
    .restart(RestartPolicy::Always);

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForOne)
        .child(press)
        .child(front_desk)
        .build()?;

    let handle = supervisor.spawn();
    match handle.wait().await {
        Ok(()) => println!("supervisor stopped cleanly"),
        Err(error) => println!("supervisor gave up: {error}"),
    }
    Ok(())
}
```

Running this prints the press restarting three times, each generation 100 ms
apart, and then the supervisor giving up because the restart intensity limit
was exceeded:

```text
press starting (generation 0)
press starting (generation 1)
press starting (generation 2)
press starting (generation 3)
supervisor gave up: restart intensity exceeded
```

Let's unpack the policies that produced that behaviour.

## Restart policies

Each child has a [`RestartPolicy`] that decides whether an exit triggers a
restart:

- **`RestartPolicy::Always`** — always restart, even after a clean `Ok(())`
  exit. Right for services that should simply never stop, like the front
  desk.
- **`RestartPolicy::OnFailure`** (the default) — restart only on failure (`Err`,
  panic, or abort). A clean exit is final. Right for the press: a jam should
  be retried, but if the press decides it is done, it is done.
- **`RestartPolicy::Never`** — never restart. Runs at most once; useful for
  one-shot startup jobs.

## Restart intensity and backoff

Unbounded restarting would turn a persistent fault into a busy loop, so
restarts are budgeted by a [`RestartIntensity`]: at most `max_restarts`
restarts within a sliding `within` window (the default is 5 restarts within
30 seconds). Exceeding the budget makes the whole supervisor exit with
`SupervisorError::RestartIntensityExceeded` — in a supervision tree, that
escalates the failure to the parent.

A [`BackoffPolicy`] optionally delays each restart attempt: `Fixed`,
`Exponential`, or `JitteredExponential`. The exponential attempt count is a
per-child consecutive-restart counter that resets once a run survives longer
than the intensity window. A shutdown request always wins over a pending
restart delay.

Intensity can be set on the supervisor as a whole
(`SupervisorBuilder::restart_intensity`) or overridden per child, as we did
for the press.

## Strategies

The [`Strategy`] decides who is affected when a child fails:

- **`Strategy::OneForOne`** (default) — only the failed child restarts. The
  front desk never notices the press jamming.
- **`Strategy::OneForAll`** — every child is stopped and restarted together.
  Use this when children hold interdependent state, e.g. a producer/consumer
  pair that must resynchronize from scratch. (`Never` children are drained
  with the group but not respawned.)
- **`Strategy::RestForOne`** — the failed child and every child declared after
  it are stopped, then eligible children in that suffix restart in declaration
  order. Earlier children remain running. Use this for ordered pipelines.

## Shutdown policies

When a child must stop — on supervisor shutdown, removal, or a group restart —
its [`ShutdownPolicy`] governs how:

- **`ShutdownPolicy::cooperative_strict(grace)`** — cancel the child's token
  and wait up to `grace` for a voluntary exit; abort *and report a timeout
  error* otherwise.
- **`ShutdownPolicy::cooperative_then_abort(grace)`** (default, 5 s grace) —
  same, but the fallback Tokio `abort()` is expected and not reported as an
  error.
- **`ShutdownPolicy::abort()`** — abort immediately.

One caveat inherited from Tokio itself: aborts take effect at `.await` points.
A child stuck in a non-yielding loop cannot be preempted — isolate truly
blocking work behind a blocking pool (as the actor layer's `run_blocking`
does, see the next chapter) or an external process.

## Automatic shutdown for finite work

Pipeline and batch subtrees often have a natural completion point. Mark those
children with `ChildSpec::significant()` and select an [`AutoShutdown`] mode on
the supervisor:

```rust,ignore
let batch = SupervisorBuilder::new()
    .auto_shutdown(AutoShutdown::AllSignificant)
    .child(source.restart(RestartPolicy::OnFailure).significant())
    .child(indexer.restart(RestartPolicy::Never).significant())
    .child(metrics_reporter)
    .build()?;
```

`AnySignificant` stops the remaining children after the first significant
child returns `Ok(())`; `AllSignificant` waits until every significant child
has returned `Ok(())`. Failures still follow the normal restart policy and do
not trigger automatic shutdown. Consequently, a significant `Never` child
that fails cannot later satisfy `AllSignificant`; the supervisor continues
until explicitly stopped.

Significant children must use `OnFailure` or `Never`, and a supervisor with
significant children must select a non-`Never` automatic shutdown mode. Nested
supervisors can be marked significant through `SupervisorSpec::significant()`,
so a completed subtree is observed by its parent as an ordinary clean child
exit.

## Supervision trees

A supervisor is a first-class child kind, giving each subsystem its own
restart budget while failures that exhaust it escalate to the parent:

```rust,ignore
let pressroom = SupervisorBuilder::new()
    .child(press) // ... the flaky press from above
    .build()?;

let shop = SupervisorBuilder::new()
    .supervisor("pressroom", pressroom)
    .child(front_desk)
    .build()?;
```

The nested supervisor forwards its lifecycle events to the parent and shows up
inside the parent's snapshots, so observability (chapter 6) sees the whole
tree.

## Dynamic children

Children can also be added and removed while the supervisor is running,
through the handle:

```rust,ignore
handle.add_child(ChildSpec::new("night-shift-press", factory)).await?;
handle.remove_child("night-shift-press").await?;

// Control a nested supervisor through its restart-stable handle:
let pressroom = handle.supervisor("pressroom").expect("configured above");
pressroom.add_child(child).await?;
```

Supervisors can start empty or have their last child removed. At zero children
they keep serving control commands and wait for the next `add_child` or an
explicit shutdown.
We will use a higher-level version of this API in the [Dynamic
actors](dynamic-actors.md) chapter.

[`ChildSpec`]: https://stokes.io/tokio-otp/api/tokio_supervisor/struct.ChildSpec.html
[`RestartPolicy`]: https://stokes.io/tokio-otp/api/tokio_supervisor/enum.RestartPolicy.html
[`RestartIntensity`]: https://stokes.io/tokio-otp/api/tokio_supervisor/struct.RestartIntensity.html
[`BackoffPolicy`]: https://stokes.io/tokio-otp/api/tokio_supervisor/enum.BackoffPolicy.html
[`Strategy`]: https://stokes.io/tokio-otp/api/tokio_supervisor/enum.Strategy.html
[`ShutdownPolicy`]: https://stokes.io/tokio-otp/api/tokio_supervisor/struct.ShutdownPolicy.html
[`AutoShutdown`]: https://stokes.io/tokio-otp/api/tokio_supervisor/enum.AutoShutdown.html
