# Actor Graphs

`tokio-actor` models a group of async actors with typed mailboxes. Each actor
declares one message type, and a topology mints restart-stable `ActorRef<M>`
handles that can be stored in other actors' state.

There are no string-addressed sends and no byte envelope type. If the front
desk sends orders to the press, the front desk owns an `ActorRef<Order>`.

The usual static graph is a struct whose fields are the actors. Deriving
`Topology` gives that struct a `graph` method; its wiring closure receives a
refs struct with one typed `ActorRef` per field, so cycles and forward
references do not require string lookup.

```rust,no_run
use tokio_actor::{ActorContext, ActorRef, ActorResult, GraphBuilder, Actor, Reply, Topology};
use tokio_otp::Runtime;

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

    async fn handle(&mut self, order: Order, _ctx: &ActorContext<Order>) -> ActorResult {
        self.press.send(order).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct Press {
    shipping: ActorRef<ShippingMsg>,
}

impl Actor for Press {
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

impl Actor for Shipping {
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

#[derive(Topology)]
struct PrintShop {
    front_desk: FrontDesk,
    press: Press,
    shipping: Shipping,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = GraphBuilder::new();
    builder.name("print-shop");

    let graph = PrintShop::graph_with(builder, |refs| PrintShop {
        front_desk: FrontDesk {
            press: refs.press.clone(),
        },
        press: Press {
            shipping: refs.shipping.clone(),
        },
        shipping: Shipping::default(),
    })?;
    let orders = graph.actor_ref::<Order>("front_desk")?;
    let shipping = graph.actor_ref::<ShippingMsg>("shipping")?;

    let handle = Runtime::builder().graph(graph).build()?.spawn();

    orders.send(Order("business cards x100".into())).await?;
    orders.send(Order("flyers x500".into())).await?;

    let shipped = shipping.call(ShippingMsg::Total).await?;
    println!("shipped {shipped} jobs");

    handle.shutdown_and_wait().await?;
    Ok(())
}
```

For lower-level hosting, iterate `graph.actors()` and drive each
`RunnableActor::run_until` independently. `tokio-otp` performs that adaptation
for the common supervised runtime.

## Struct Topologies

`#[derive(Topology)]` supports named-field structs whose fields implement
`RawActor`. Field names become actor ids verbatim, so supervisor child ids,
runtime lookups, tracing fields, and metric labels stay human-readable without
participating in type checking. The generated `graph_with` accepts a
preconfigured `GraphBuilder` for graph name, mailbox capacity, and shutdown
timeouts; the generated `graph` uses `GraphBuilder::new()`.

The derive keeps topology shape in the type system:

- a field whose type is not an actor is a compile error
- wiring a ref with the wrong message type is a compile error
- filling the same field twice is impossible because the generated code owns
  one actor value per field
- a topology with no actors is a compile error

## Dynamic and Advanced Builder Wiring

Use `GraphBuilder` directly when actors are dynamic, generated in a loop, or
need explicit observability names:

- `builder.add(actor)` registers an actor under its unqualified type name,
  suffixing repeats as `Worker-2`, `Worker-3`, and so on
- `builder.actor(id, actor)` registers an actor under an explicit id
- `builder.slot::<M>(id)` plus `builder.define(slot, actor)` opens and fills a
  token-protected slot for hand-written cyclic wiring

The direct builder still validates runtime configuration facts:

- duplicate actor ids
- slots that were opened but never filled
- empty graph names, empty actor ids, and zero mailbox capacity

## Runtime Handles

`ActorRef<M>` is the external entry point. It supports:

| Method | Behavior |
|--------|----------|
| `send` | Waits for a bound mailbox, waits for capacity, and retries across expected restart windows. |
| `try_send` | Returns immediately if the actor is unbound, terminated, full, or closed. |
| `call` | Sends a message carrying `Reply<T>` and awaits the reply. |

Refs are bound to long-lived mailbox bindings, not one actor incarnation. A
ref minted before `build()` keeps working across graph reruns and per-actor
restarts under `tokio-otp`.

## Message Loss at Shutdown and Restart

`Actor` is the usual actor interface: you implement `handle`, and the
framework owns the receive loop. Its default shutdown behavior is fail-fast:
when shutdown is requested, queued messages are dropped and queued `call`
requests see `CallError::ReplyDropped`.

If an actor must finish messages already accepted by its mailbox, return
`DrainPolicy::Drain` from `drain_policy`. Hand-written `RawActor::run` loops are
still available as the escape hatch for custom loop control; after
`ctx.recv().await` returns `None` because shutdown was requested, such actors
can use `ctx.try_recv()` to drain immediately queued messages.

Restarts have the same loss boundary. Each actor run binds a fresh mailbox, so
messages queued behind the message that makes an actor crash are lost with the
old mailbox. `send` retries while an actor is between bindings, but
it cannot recover messages that were already accepted by the old run.

## Blocking Work

Actors can offload blocking work through `ctx.run_blocking`. Its closure
receives a cancellation token that follows actor shutdown and is also
cancelled if the `run_blocking` future is dropped. Long-running closures
should check it periodically.

```rust,ignore
let engraved = ctx.run_blocking(move |token| {
    if token.is_cancelled() {
        return None;
    }
    Some(engrave(input))
}).await;
```

The closure can return any type, including an application-defined `Result`.
A panic resumes on the actor task. If a closure ignores cancellation, the
actor shutdown timeout remains the backstop; the blocking thread then runs
detached because Tokio cannot abort blocking work after it starts.

For intentionally detached or concurrent work, clone `ctx.myself()`, call
`tokio::task::spawn_blocking` directly, and send the result back as a message.
The `blocking_lifecycle` example demonstrates this mailbox-as-completion
pattern.

[`ActorRef`]: https://stokes.io/tokio-otp/api/tokio_actor/struct.ActorRef.html
