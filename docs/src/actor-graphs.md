# Actor graphs

Supervising opaque tasks is useful, but our print shop is really a *pipeline*:
orders flow from the front desk to the press to shipping. `tokio-actor` models
exactly that — a static graph of actors with directed links between their
mailboxes, plus named **ingress** points where the outside world can inject
messages.

`tokio-actor` deliberately does *not* handle restarts. When any actor fails,
the whole graph run fails. That sounds harsh, but it is a feature: it makes a
graph (or a single actor, as we'll see in the next chapter) a clean unit for
`tokio-supervisor` to restart.

## The print shop as a graph

```rust,no_run
use std::time::Duration;

use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let graph = GraphBuilder::new()
        .name("print-shop")
        .actor(ActorSpec::from_actor(
            "front-desk",
            |mut ctx: ActorContext| async move {
                while let Some(order) = ctx.recv().await {
                    if order.as_slice().is_empty() {
                        println!("front-desk: rejected an empty order");
                        continue;
                    }
                    ctx.send("press", order).await?;
                }
                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor(
            "press",
            |mut ctx: ActorContext| async move {
                while let Some(order) = ctx.recv().await {
                    let job = String::from_utf8_lossy(order.as_slice()).into_owned();
                    let printed = format!("printed[{job}]");
                    ctx.send("shipping", Envelope::from(printed.into_bytes()))
                        .await?;
                }
                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor(
            "shipping",
            |mut ctx: ActorContext| async move {
                while let Some(parcel) = ctx.recv().await {
                    println!(
                        "shipping: {}",
                        String::from_utf8_lossy(parcel.as_slice())
                    );
                }
                Ok(())
            },
        ))
        .link("front-desk", "press")
        .link("press", "shipping")
        .ingress("orders", "front-desk")
        .build()?;

    // Ingress handles are grabbed from the graph *spec* and stay valid
    // across runs.
    let mut orders = graph.ingress("orders").expect("ingress exists");

    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    orders.wait_for_binding().await;
    orders.send(Envelope::from_static(b"business cards x100")).await?;
    orders.send(Envelope::from_static(b"flyers x500")).await?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.cancel();
    run.await??;
    Ok(())
}
```

Output:

```text
shipping: printed[business cards x100]
shipping: printed[flyers x500]
```

## The pieces

**[`GraphBuilder`]** declares actors, `link`s (which actor may send to which),
and `ingress` points, and validates the whole thing at `build()` time —
unknown link targets, duplicate ids, or an ingress pointing at a missing actor
are build errors, not runtime surprises. The built [`Graph`] is an immutable,
cloneable *spec*: `run_until` can be called on clones again and again.

**[`ActorSpec::from_actor`]** accepts either a closure taking an
[`ActorContext`], as above, or any type implementing the [`Actor`] trait —
handy once an actor grows real state:

```rust,ignore
#[derive(Clone)]
struct Press {
    ink_level: u8,
}

impl Actor for Press {
    async fn run(&self, mut ctx: ActorContext) -> ActorResult {
        while let Some(order) = ctx.recv().await { /* ... */ }
        Ok(())
    }
}
```

**[`ActorContext`]** is an actor's window on the world: `recv()` reads the
mailbox, `send(peer_id, envelope)` delivers to a *linked* peer,
`shutdown_token()` signals graceful shutdown, and `peer(id)` hands out a
cloneable [`ActorRef`] for use outside the actor loop.

**[`Envelope`]** is an opaque, cheaply-cloneable byte payload (backed by
`Bytes`). Serialization is your business — actors just move bytes.

**[`IngressHandle`]** is the stable external entry point. `wait_for_binding()`
parks until the target actor's mailbox is actually live — important because
the handle exists before the graph runs, and *keeps working across restarts*
(more on that below).

## Send variants and back-pressure

Mailboxes are bounded (64 messages by default), so sending applies
back-pressure. There are a few flavors, on both `ActorContext` and
`IngressHandle`:

| Method | Behaviour when the mailbox is full or the target is down |
|--------|-----------------------------------------------------------|
| `send` | Waits for mailbox space; fails if the current binding is closed. |
| `send_when_ready` | Like `send`, but if the target is down it waits for the *next* binding and retries — the right choice when a supervisor may be restarting the target. |
| `try_send` | Never waits; returns an error immediately. |

## Blocking work

CPU-heavy or genuinely blocking work must not run on the async executor. An
actor can push it onto a tracked blocking pool:

```rust,ignore
let sink = ctx.peer("shipping").expect("linked peer");
let _handle = ctx.spawn_blocking(BlockingOptions::named("engrave"), move |job| {
    for step in 0..5 {
        job.checkpoint()?; // observe cancellation between steps
        do_heavy_step(step);
    }
    sink.blocking_send(result)?; // ActorRef works from blocking threads too
    Ok(())
})?;
```

The graph tracks these tasks: per-actor concurrency is capped (16 by default)
and shutdown waits up to 5 seconds for them before detaching. Call
`checkpoint()` regularly so graceful shutdown actually is graceful. There is
also `ctx.run_blocking(...)` when the actor wants to await the result inline.

## Resource limits

Defaults are conservative and tunable per graph:

```rust,ignore
let graph = GraphBuilder::new()
    .mailbox_capacity(256)                              // default 64
    .max_envelope_bytes(64 * 1024)                      // default 1 MiB
    .max_blocking_tasks_per_actor(4)                    // default 16
    .blocking_shutdown_timeout(Duration::from_secs(2))  // default 5 s
    // ...
    .build()?;
```

## Why handles surviving restarts matters

Here is the property that makes the next chapter work: `ActorRef` and
`IngressHandle` are bound to long-lived mailbox *bindings*, not to one actor
incarnation. When a graph is rerun — or a single actor is restarted from the
same wiring — existing handles transparently follow the new mailbox. Customers
holding the `orders` ingress never need to know the press was replaced.

We could now wrap the whole graph in a `ChildSpec` by hand and let a
supervisor rerun it on failure. But restarting the *entire shop* because the
press jammed is heavy-handed. The `tokio-otp` crate gives us something better.

[`GraphBuilder`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
[`Graph`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
[`ActorSpec::from_actor`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
[`Actor`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
[`ActorContext`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
[`ActorRef`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
[`Envelope`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
[`IngressHandle`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
