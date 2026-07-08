# Supervised Actors

`tokio-otp` decomposes a typed actor graph so each actor becomes its own
supervised child. Existing `ActorRef<M>` handles keep following the same
long-lived mailbox bindings, so a sender can wait while a failed actor is
restarted and then deliver to the new generation.

```rust,no_run
use std::{io, sync::{Arc, atomic::{AtomicUsize, Ordering}}, time::Duration};

use tokio_actor::{Actor, ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{Restart, RestartIntensity, Strategy, SupervisorBuilder};

#[derive(Clone)]
struct FrontDesk {
    press: ActorRef<String>,
}

impl Actor for FrontDesk {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(order) = ctx.recv().await {
            let mut press = self.press.clone();
            press.send_when_ready(order).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Press {
    runs: Arc<AtomicUsize>,
}

impl Actor for Press {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        while let Some(order) = ctx.recv().await {
            if run == 0 && order.contains("origami") {
                return Err::<(), BoxError>(Box::new(io::Error::other("paper jam")));
            }
            println!("printed {order}");
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = GraphBuilder::new();
    let press_ref = builder.declare::<String>("press");
    let mut orders = builder.actor("front-desk", FrontDesk { press: press_ref });
    builder.actor("press", Press { runs: Arc::new(AtomicUsize::new(0)) });
    let graph = builder.build()?;

    let runtime = SupervisedActors::new(graph)?
        .restart(Restart::Transient)
        .actor_restart_intensity("press", RestartIntensity::new(5, Duration::from_secs(60)))
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();

    orders.send_when_ready("business cards x100".into()).await?;
    orders.send_when_ready("origami cranes x1000".into()).await?;
    orders.send_when_ready("flyers x500".into()).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;
    handle.shutdown_and_wait().await?;
    Ok(())
}
```

`SupervisedActors::new(graph)` calls `Graph::into_actor_set`, then you choose
the integration level:

- `build_runtime(builder)` returns a `Runtime` with a supervisor, registry,
  and dynamic actor support.
- `build_supervisor(builder)` returns a plain `Supervisor`.
- `build()` returns `Vec<ChildSpec>` for integrating actor children into a
  larger supervisor manually.

`RuntimeHandle::actor_ref::<M>(id)` performs a typed registry lookup. A
mismatched message type returns `LookupError::MessageTypeMismatch`.

Whole-graph supervision is still available through `SupervisedGraph` when a
group of actors should restart as one child.
