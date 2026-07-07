# Supervised actors

This is the payoff chapter. The `tokio-otp` crate takes the actor graph from
the previous chapter and decomposes it so that **each actor becomes its own
supervised child** — the press can jam and be replaced while the front desk
and shipping keep running, and the `orders` ingress handle keeps working the
whole time.

## The supervised print shop

```rust,no_run
use std::time::Duration;

use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{
    BackoffPolicy, Restart, RestartIntensity, Strategy, SupervisorBuilder,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let graph = GraphBuilder::new()
        .name("print-shop")
        .actor(ActorSpec::from_actor(
            "front-desk",
            |mut ctx: ActorContext| async move {
                while let Some(order) = ctx.recv().await {
                    // `send_when_ready` retries across press restarts.
                    ctx.send_when_ready("press", order).await?;
                }
                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor(
            "press",
            |mut ctx: ActorContext| async move {
                println!("press: online");
                while let Some(order) = ctx.recv().await {
                    let job = String::from_utf8_lossy(order.as_slice()).into_owned();
                    if job.contains("origami") {
                        // This press cannot fold. Crash and let the
                        // supervisor sort it out.
                        return Err(format!("paper jam on `{job}`").into());
                    }
                    let printed = format!("printed[{job}]");
                    ctx.send_when_ready("shipping", Envelope::from(printed.into_bytes()))
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

    // Decompose the graph into per-actor children and build the integrated
    // runtime: a supervisor plus stable ingress handles.
    let runtime = SupervisedActors::new(graph)?
        .actor_restart("front-desk", Restart::Permanent)
        .actor_restart_intensity(
            "press",
            RestartIntensity::new(5, Duration::from_secs(60))
                .with_backoff(BackoffPolicy::Fixed(Duration::from_millis(100))),
        )
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;

    let handle = runtime.spawn();

    let mut orders = handle.ingress("orders").expect("ingress exists");
    orders.wait_for_binding().await;

    orders.send(Envelope::from_static(b"business cards x100")).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    orders.send(Envelope::from_static(b"origami cranes x1000")).await?; // jams the press
    tokio::time::sleep(Duration::from_millis(100)).await;

    orders.send(Envelope::from_static(b"flyers x500")).await?; // delivered after the restart

    tokio::time::sleep(Duration::from_secs(1)).await;
    handle.shutdown_and_wait().await?;
    Ok(())
}
```

Output — note the second `press: online` and that the flyers survive the jam:

```text
press: online
shipping: printed[business cards x100]
press: online
shipping: printed[flyers x500]
```

The origami order crashed the press; the supervisor restarted only the press
(`OneForOne`), the front desk's `send_when_ready` transparently followed the
new mailbox binding and delivered the flyers, and shipping never noticed a
thing. The origami order itself is gone — that is the "let it crash"
trade-off: a message that makes an actor fail dies with that actor's mailbox
instead of poisoning every future generation. (If an order must survive a
jam, persist it outside the graph and re-inject it.)

One practical note on the pauses between sends: messages already sitting in a
crashed actor's mailbox are lost with it. Without the first sleep, the flyers
order could race into the *old* press mailbox before the origami order jammed
it, and be lost alongside it. `send_when_ready` protects the sends that
happen while the target is down; it cannot recover what was already queued.

## What `SupervisedActors` does

[`SupervisedActors::new(graph)`] internally calls `Graph::into_actor_set`,
splitting the validated graph into independently runnable actors that still
share their original wiring. Then you choose how much integration you want:

- **`build_runtime(supervisor_builder)`** → a [`Runtime`] owning the
  supervisor *and* the graph's ingress handles. `runtime.spawn()` returns a
  [`RuntimeHandle`] combining both control surfaces. This is the common path,
  used above.
- **`build_supervisor(supervisor_builder)`** → a plain
  `(Supervisor, HashMap<String, IngressHandle>)`, when you want to spawn and
  wire things yourself.
- **`build()`** → raw `(Vec<ChildSpec>, HashMap<String, IngressHandle>)`,
  when the actors should join a supervisor that also has non-actor children.

## Per-actor policies

Every knob from the [supervision chapter](supervision.md) applies per actor:

```rust,ignore
let supervised = SupervisedActors::new(graph)?
    // Defaults for all actors:
    .restart(Restart::Transient)
    .shutdown(ShutdownPolicy::cooperative_then_abort(Duration::from_secs(5)))
    // Per-actor overrides:
    .actor_restart("front-desk", Restart::Permanent)
    .actor_restart_intensity("press", RestartIntensity::new(5, Duration::from_secs(60)))
    .actor_shutdown("shipping", ShutdownPolicy::cooperative(Duration::from_secs(10)));
```

## The `RuntimeHandle`

The handle merges the supervisor's control plane with the graph's data plane:

```rust,ignore
let orders = handle.ingress("orders");          // stable ingress handle
let press = handle.actor_ref("press");          // stable ActorRef to any actor
let sup = handle.supervisor_handle();           // raw SupervisorHandle if needed

let events = handle.subscribe();                // lifecycle events (chapter 6)
let snapshot = handle.snapshot();               // current tree state

handle.shutdown();                              // request shutdown...
let exit = handle.shutdown_and_wait().await?;   // ...or request and await it
```

## The alternative: whole-graph supervision

Sometimes actors are so interdependent that restarting them individually makes
no sense — you *want* the whole shop rebooted on any failure. That is
[`SupervisedGraph`]: the entire graph runs as one child.

```rust,ignore
use tokio_otp::SupervisedGraph;

let supervised = SupervisedGraph::new("print-shop", graph);
let orders = supervised.ingress("orders").expect("ingress exists");

let supervisor = SupervisorBuilder::new()
    .child(supervised.into_child_spec()) // apply .restart(...) etc. as usual
    .build()?;
```

It composes with anything else the supervisor manages, and the same
stable-ingress guarantee applies across whole-graph reruns. Rule of thumb:
start with `SupervisedActors` and reach for `SupervisedGraph` when a group of
actors shares state that cannot survive a partial restart — it is the
`OneForAll` of the actor world.

[`SupervisedActors::new(graph)`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-otp
[`Runtime`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-otp
[`RuntimeHandle`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-otp
[`SupervisedGraph`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-otp
