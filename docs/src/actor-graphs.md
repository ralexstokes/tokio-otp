# Actor Graphs

`tokio-actor` models a group of async actors with typed mailboxes. Each actor
declares one message type, and the graph builder mints restart-stable
`ActorRef<M>` handles that can be stored in other actors' state.

There are no string-addressed sends and no byte envelope type. If the front
desk sends orders to the press, the front desk owns an `ActorRef<Order>`.

```rust,no_run
use tokio_actor::{ActorContext, ActorRef, ActorResult, GraphBuilder, MessageHandler, Reply};

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

impl MessageHandler for FrontDesk {
    type Msg = Order;

    async fn handle(&mut self, order: Order, _ctx: &ActorContext<Order>) -> ActorResult {
        self.press.send(order).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct Press {
    shipping: ActorRef<ShippingMsg>,
}

impl MessageHandler for Press {
    type Msg = Order;

    async fn handle(&mut self, Order(order): Order, _ctx: &ActorContext<Order>) -> ActorResult {
        self.shipping
            .send(ShippingMsg::Ship(Parcel(format!("printed[{order}]"))))
            .await?;
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Shipping {
    shipped: usize,
}

impl MessageHandler for Shipping {
    type Msg = ShippingMsg;

    async fn handle(
        &mut self,
        message: ShippingMsg,
        _ctx: &ActorContext<ShippingMsg>,
    ) -> ActorResult {
        match message {
            ShippingMsg::Ship(Parcel(parcel)) => {
                self.shipped += 1;
                println!("shipping: {parcel}");
            }
            ShippingMsg::Total(reply) => reply.send(self.shipped),
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
    let orders = builder.actor("front-desk", FrontDesk { press: press.clone() });
    builder.actor("press", Press { shipping: shipping.clone() });
    builder.actor("shipping", Shipping::default());
    let graph = builder.build()?;

    let handle = graph.spawn()?;

    orders.send(Order("business cards x100".into())).await?;
    orders.send(Order("flyers x500".into())).await?;

    let shipped = shipping.call(ShippingMsg::Total).await?;
    println!("shipped {shipped} jobs");

    handle.shutdown_and_wait().await?;
    Ok(())
}
```

`Graph::run_until` remains available when you want to drive a graph inside
your own task or `select!` loop. `tokio-otp` uses that lower-level API for
supervised graph children.

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

## Message Loss at Shutdown and Restart

`MessageHandler` is the usual actor interface: you implement `handle`, and the
framework owns the receive loop. Its default shutdown behavior is fail-fast:
when shutdown is requested, queued messages are dropped and queued `call`
requests see `CallError::ReplyDropped`.

If an actor must finish messages already accepted by its mailbox, return
`DrainPolicy::Drain` from `drain_policy`. Hand-written `Actor::run` loops are
still available as the escape hatch for custom loop control; after
`ctx.recv().await` returns `None` because shutdown was requested, such actors
can use `ctx.try_recv()` to drain immediately queued messages.

Restarts have the same loss boundary. Each actor run binds a fresh mailbox, so
messages queued behind the message that makes an actor crash are lost with the
old mailbox. `send_when_ready` retries while an actor is between bindings, but
it cannot recover messages that were already accepted by the old run.

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
