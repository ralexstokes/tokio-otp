# Observability

A supervisor that silently restarts things is a great way to hide problems.
The crates offer four complementary windows into a running tree, from
programmatic to purely visual:

1. **Events** — react to lifecycle changes in code.
2. **Snapshots** — poll or watch the current state of the whole tree.
3. **`tracing` / `metrics`** — structured logs and dashboard-friendly counters.
4. **The web console** — watch it live in a browser.

## Events and snapshots

Here is the supervised print shop again, now with an observer that logs every
restart of the press and inspects the tree state on demand:

```rust,no_run
use std::time::Duration;

use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{Strategy, SupervisorBuilder, SupervisorEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let graph = GraphBuilder::new()
        .name("print-shop")
        .actor(ActorSpec::from_actor(
            "press",
            |mut ctx: ActorContext| async move {
                while let Some(order) = ctx.recv().await {
                    if order.as_slice().starts_with(b"origami") {
                        return Err("paper jam".into());
                    }
                    println!("printed {}", String::from_utf8_lossy(order.as_slice()));
                }
                Ok(())
            },
        ))
        .ingress("orders", "press")
        .build()?;

    let runtime = SupervisedActors::new(graph)?
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();

    // 1. Events: a broadcast channel of lifecycle changes.
    let mut events = handle.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                SupervisorEvent::ChildExited { id, status, .. } => {
                    println!("observer: {id} exited: {status:?}");
                }
                SupervisorEvent::ChildRestarted {
                    id, new_generation, ..
                } => {
                    println!("observer: {id} restarted (generation {new_generation})");
                }
                _ => {}
            }
        }
    });

    let mut orders = handle.ingress("orders").expect("ingress exists");
    orders.wait_for_binding().await;
    orders.send(Envelope::from_static(b"origami cranes")).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // 2. Snapshots: the current state of every child, on demand.
    for child in &handle.snapshot().children {
        println!(
            "snapshot: {} generation={} state={:?} restarts={}",
            child.id, child.generation, child.state, child.restart_count
        );
    }

    handle.shutdown_and_wait().await?;
    Ok(())
}
```

Output:

```text
observer: press exited: Failed("actor `press` returned an error")
observer: press restarted (generation 1)
snapshot: press generation=1 state=Running restarts=1
observer: press exited: Completed
```

**Events** ([`SupervisorEvent`]) arrive on a `tokio::sync::broadcast` channel:
`ChildStarted`, `ChildExited`, `ChildRestartScheduled`, `ChildRestarted`,
`ChildRemoved`, `RestartIntensityExceeded`, `SupervisorStopped`, and more.
Events from nested supervisors are forwarded to the parent wrapped in
`SupervisorEvent::Nested`. Broadcast receivers that fall behind lose the
oldest events (that's a `Lagged` error on `recv`), so treat events as
notifications, not as a durable log.

**Snapshots** ([`SupervisorSnapshot`]) are the complement: not a stream of
changes but the *current* state — every child's generation, state
(starting/running/stopping/stopped), restart count, time until the next
restart attempt, and last exit status, recursively including nested
supervisors. `handle.snapshot()` reads it once;
`handle.subscribe_snapshots()` returns a `watch::Receiver` for "wait until the
tree looks like X" logic:

```rust,ignore
let mut snapshots = handle.subscribe_snapshots();
loop {
    snapshots.changed().await?;
    if snapshots.borrow().children.iter().all(|c| c.state == ChildStateView::Running) {
        break; // steady state reached
    }
}
```

A useful guarantee: the snapshot is updated *before* the corresponding event
is broadcast, so an event handler that reads `handle.snapshot()` always sees
state at least as new as the event it is reacting to.

## `tracing` and `metrics`

Every lifecycle transition is also emitted as structured `tracing` output —
the supervisor runs inside an `info_span!("supervisor")` and each child inside
an `info_span!("child")` carrying `supervisor_name`, `child_id`, and
`generation` fields. There is nothing to enable; just install a subscriber at
your application boundary:

```rust,ignore
tracing_subscriber::fmt()
    .with_max_level(tracing::Level::INFO)
    .init();
```

With the optional `metrics` cargo feature (on `tokio-supervisor`,
`tokio-actor`, or transitively via `tokio-otp`), the crates also emit
low-cardinality counters, gauges, and histograms — e.g.
`supervisor.children.running`, `supervisor.restarts`,
`supervisor.child_shutdown.duration` — through the
[`metrics`](https://docs.rs/metrics) facade, ready for a Prometheus exporter.
See `examples/metrics.rs` in either crate for an end-to-end setup.

## The web console

Finally, the `console` feature of `tokio-otp` (backed by the
`tokio-otp-console` crate) serves a live view of the supervision tree over
HTTP, fed by the same snapshots and events:

```rust,ignore
// Cargo.toml: tokio-otp = { ..., features = ["console"] }
let handle = runtime.spawn();

let console = handle
    .console()
    .bind(([127, 0, 0, 1], 8080))
    .build()
    .spawn()
    .await?;

println!("console at http://{}", console.local_addr());
// ...
console.shutdown();
```

Run `cargo run -p tokio-otp --example console --features console` from the
repository and open the printed address to watch a deliberately flaky worker
restart in real time.

[`SupervisorEvent`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-supervisor
[`SupervisorSnapshot`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-supervisor
