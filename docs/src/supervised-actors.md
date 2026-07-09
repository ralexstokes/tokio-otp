# Supervised Actors

`tokio-otp` decomposes a typed actor graph so each actor becomes its own
supervised child. Existing `ActorRef<M>` handles keep following the same
long-lived mailbox bindings, so a sender can wait while a failed actor is
restarted and then deliver to the new generation.

```rust,no_run
use std::{io, sync::{Arc, atomic::{AtomicUsize, Ordering}}, time::Duration};

use tokio_otp::prelude::*;

#[derive(Clone)]
struct FrontDesk {
    press: ActorRef<String>,
}

impl MessageHandler for FrontDesk {
    type Msg = String;

    async fn handle(&mut self, order: String, _ctx: &ActorContext<String>) -> ActorResult {
        self.press.send(order).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct Press {
    runs: Arc<AtomicUsize>,
    run: usize,
}

impl MessageHandler for Press {
    type Msg = String;

    async fn on_start(&mut self, _ctx: &ActorContext<String>) -> ActorResult {
        self.run = self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, order: String, _ctx: &ActorContext<String>) -> ActorResult {
        if self.run == 0 && order.contains("origami") {
            return Err::<(), BoxError>(Box::new(io::Error::other("paper jam")));
        }
        println!("printed {order}");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = GraphBuilder::new();
    let (press_slot, press_ref) = builder.slot::<String>("press");
    let orders = builder.actor("front-desk", FrontDesk { press: press_ref });
    builder.define(press_slot, Press { runs: Arc::new(AtomicUsize::new(0)), run: 0 });
    let graph = builder.build()?;

    let runtime = Runtime::builder()
        .graph(graph)
        .strategy(Strategy::OneForOne)
        .restart(Restart::Transient)
        .restart_intensity(RestartIntensity::new(5, Duration::from_secs(60)))
        .build()?;
    let handle = runtime.spawn();

    orders.send("business cards x100".into()).await?;
    let restart = handle.monitor_restart("press")?;
    orders.send("origami cranes x1000".into()).await?;
    restart.await?;
    orders.send("flyers x500".into()).await?;

    handle.shutdown_and_wait().await?;
    Ok(())
}
```

`Runtime::builder()` is the front door for the common case: it decomposes the
graph, applies uniform policies to every actor child, and packages the result
into a `Runtime` with a supervisor, registry, and dynamic actor support.

The restart monitor before sending `origami cranes x1000` is still deliberate.
A worker gets a fresh mailbox on restart; anything queued behind the crashing
`origami` order would be dropped with the old mailbox. `send` waits while the
actor is unbound, but it cannot recover messages already accepted by the
failed run.

When you need per-actor policies — say a tighter restart budget for the press
alone — drop down to `SupervisedActors`, which the builder uses under the
hood:

```rust,ignore
let runtime = SupervisedActors::new(graph)?
    .restart(Restart::Transient)
    .actor_restart_intensity("press", RestartIntensity::new(5, Duration::from_secs(60)))
    .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
```

`SupervisedActors::new(graph)` calls `Graph::into_actor_set`, then you choose
the integration level:

- `build_runtime(builder)` returns a `Runtime` with a supervisor, registry,
  and dynamic actor support.
- `build_supervisor(builder)` returns a plain `Supervisor`.
- `build()` returns `Vec<ChildSpec>` for integrating actor children into a
  larger supervisor manually.

`RuntimeHandle::actor_ref::<M>(id)` performs a typed registry lookup. Lookup
failures such as `LookupError::MessageTypeMismatch` arrive wrapped in
`DynamicActorError::Lookup`, and a runtime built without a dynamic registry
returns `DynamicActorError::Unsupported`.

Whole-graph supervision is still available through `SupervisedGraph` when a
group of actors should restart as one child.
