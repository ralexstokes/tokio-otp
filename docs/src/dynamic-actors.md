# Dynamic Actors

`RuntimeHandle::add_actor` adds a new typed actor to a running
`SupervisedActors` runtime. The actor is registered in the runtime
`ActorRegistry`, so other actors can discover it with a typed lookup.

```rust,no_run
use tokio_actor::{ActorContext, ActorResult, GraphBuilder, MessageHandler};
use tokio_otp::{DynamicActorOptions, SupervisedActors};
use tokio_supervisor::{Strategy, SupervisorBuilder};

#[derive(Clone)]
struct FrontDesk;

impl MessageHandler for FrontDesk {
    type Msg = String;

    async fn handle(&mut self, order: String, ctx: &ActorContext<String>) -> ActorResult {
        let mut rush = ctx
            .registry()
            .expect("registry installed")
            .actor_ref::<String>("rush-press")?;
        rush.send_when_ready(order).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct RushPress;

impl MessageHandler for RushPress {
    type Msg = String;

    async fn handle(&mut self, order: String, _ctx: &ActorContext<String>) -> ActorResult {
        println!("RUSH printed {order}");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = GraphBuilder::new();
    let mut orders = builder.actor("front-desk", FrontDesk);
    let graph = builder.build()?;

    let runtime = SupervisedActors::new(graph)?
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();

    let mut rush = handle
        .add_actor("rush-press", RushPress, DynamicActorOptions::default())
        .await?;

    orders.send_when_ready("wedding invites x50".into()).await?;
    rush.send_when_ready("vip banners x2".into()).await?;

    handle.remove_actor("rush-press").await?;
    handle.shutdown_and_wait().await?;
    Ok(())
}
```

`DynamicActorOptions` carries the new child's restart policy, shutdown policy,
and optional restart intensity. Static peer lists are gone; dynamic discovery
is explicit through `ctx.registry()`.

`add_actor` returns an `ActorRef<A::Msg>` for the new actor. The same ref keeps
working across restarts of that dynamic actor. `remove_actor` shuts the child
down and removes the registry entry.
