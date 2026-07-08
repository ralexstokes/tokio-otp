# Actor Graphs

`tokio-actor` models a group of async actors with typed mailboxes. Each actor
declares one message type, and the graph builder mints restart-stable
`ActorRef<M>` handles that can be stored in other actors' state.

There are no string-addressed sends and no byte envelope type. If the front
desk sends orders to the press, the front desk owns an `ActorRef<Order>`.

```rust,no_run
use tokio_actor::{Actor, ActorContext, ActorRef, ActorResult, GraphBuilder, Reply};
use tokio_util::sync::CancellationToken;

struct Order(String);
struct Parcel(String);

enum ShippingMsg {
    Ship(Parcel),
    Total(Reply<usize>),
}

#[derive(Clone)]
struct FrontDesk {
    press: ActorRef<Order>,
}

impl Actor for FrontDesk {
    type Msg = Order;

    async fn run(&self, mut ctx: ActorContext<Order>) -> ActorResult {
        while let Some(order) = ctx.recv().await {
            self.press.send(order).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Press {
    shipping: ActorRef<ShippingMsg>,
}

impl Actor for Press {
    type Msg = Order;

    async fn run(&self, mut ctx: ActorContext<Order>) -> ActorResult {
        while let Some(Order(order)) = ctx.recv().await {
            self.shipping
                .send(ShippingMsg::Ship(Parcel(format!("printed[{order}]"))))
                .await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Shipping;

impl Actor for Shipping {
    type Msg = ShippingMsg;

    async fn run(&self, mut ctx: ActorContext<ShippingMsg>) -> ActorResult {
        let mut shipped = 0;
        while let Some(message) = ctx.recv().await {
            match message {
                ShippingMsg::Ship(Parcel(parcel)) => {
                    shipped += 1;
                    println!("shipping: {parcel}");
                }
                ShippingMsg::Total(reply) => reply.send(shipped),
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = GraphBuilder::new();
    builder.name("print-shop");

    let shipping = builder.declare::<ShippingMsg>("shipping");
    let press = builder.declare::<Order>("press");
    let mut orders = builder.actor("front-desk", FrontDesk { press: press.clone() });
    builder.actor("press", Press { shipping: shipping.clone() });
    builder.actor("shipping", Shipping);
    let graph = builder.build()?;

    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    orders.wait_for_binding().await;
    orders.send(Order("business cards x100".into())).await?;
    orders.send(Order("flyers x500".into())).await?;

    let shipped = shipping.call(ShippingMsg::Total).await?;
    println!("shipped {shipped} jobs");

    stop.cancel();
    run.await??;
    Ok(())
}
```

## Builder-Time Wiring

`GraphBuilder::actor(id, actor)` registers an actor and returns its typed
`ActorRef<A::Msg>`. `GraphBuilder::declare::<M>(id)` creates a ref before the
actor is registered, which solves cycles and forward references.

The builder validates:

- duplicate actor ids
- message type mismatches for a declared id
- declared actors that were never registered
- empty graph names, empty actor ids, and zero mailbox capacity

## Runtime Handles

`ActorRef<M>` is the external entry point. It supports:

| Method | Behavior |
|--------|----------|
| `send` | Waits for mailbox capacity on the current binding. |
| `try_send` | Returns immediately if the mailbox is full or unbound. |
| `blocking_send` | Non-async try-send semantics for blocking threads. |
| `send_when_ready` | Waits across restart windows and retries on the next binding. |
| `call` | Sends a message carrying `Reply<T>` and awaits the reply. |

Refs are bound to long-lived mailbox bindings, not one actor incarnation. A
ref minted before `build()` keeps working across graph reruns and per-actor
restarts under `tokio-otp`.

## Blocking Work

Actors can offload blocking work through `ctx.spawn_blocking` or
`ctx.run_blocking`. The blocking context carries a typed `myself()` ref, so a
blocking task can report results back to the actor's own mailbox without
serialization.

```rust,ignore
ctx.run_blocking(BlockingOptions::named("engrave"), |job| {
    job.checkpoint()?;
    job.myself().blocking_send(MyMsg::Engraved)?;
    Ok(())
}).await?;
```

Defaults are conservative: mailbox capacity is 64 messages, blocking tasks
are capped at 16 per actor, and shutdown waits up to 5 seconds for blocking
tasks before detaching them.

[`ActorRef`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
