# Dynamic Actors

A runtime does not need a graph at all: `Runtime::builder().build()` starts
empty and idles until `RuntimeHandle::add_actor` adds a typed actor. Each
added actor becomes a supervised child whose id is the actor's label, and
`add_actor` returns the typed `ActorRef<M>` directly — there is no registry
and no string lookup. Refs travel the way any other value does: cloned into
actor state at construction, or delivered by message.

```rust,no_run
use tokio_otp::{Actor, ActorContext, ActorRef, ActorResult, DynamicActorOptions, Runtime};
use tokio_supervisor::Strategy;

#[derive(Clone)]
struct FrontDesk {
    rush: Option<ActorRef<String>>,
}

enum FrontDeskMsg {
    SetRushPress(ActorRef<String>),
    Order(String),
}

impl Actor for FrontDesk {
    type Msg = FrontDeskMsg;

    async fn handle(
        &mut self,
        message: FrontDeskMsg,
        _ctx: &ActorContext<FrontDeskMsg>,
    ) -> ActorResult {
        match message {
            FrontDeskMsg::SetRushPress(rush) => self.rush = Some(rush),
            FrontDeskMsg::Order(order) => {
                self.rush
                    .as_ref()
                    .expect("rush press ref delivered before orders")
                    .send(order)
                    .await?;
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RushPress;

impl Actor for RushPress {
    type Msg = String;

    async fn handle(&mut self, order: String, _ctx: &ActorContext<String>) -> ActorResult {
        println!("RUSH printed {order}");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = Runtime::builder().strategy(Strategy::OneForOne).build()?;
    let handle = runtime.spawn();

    let orders = handle
        .add_actor("front-desk", FrontDesk { rush: None }, DynamicActorOptions::default())
        .await?;
    let rush = handle
        .add_actor("rush-press", RushPress, DynamicActorOptions::default())
        .await?;

    // Distribute the ref by message — the mailbox is the discovery channel.
    orders.send(FrontDeskMsg::SetRushPress(rush.clone())).await?;
    orders.send(FrontDeskMsg::Order("wedding invites x50".into())).await?;
    rush.send("vip banners x2".into()).await?;

    handle.remove_child("front-desk").await?;
    handle.remove_child("rush-press").await?;
    handle.shutdown_and_wait().await?;
    Ok(())
}
```

`DynamicActorOptions` carries the new child's restart policy, shutdown policy,
and optional restart intensity. `add_actor` returns an `ActorRef<A::Msg>` for
the new actor, and the same ref keeps working across restarts of that actor.

Removal is plain supervisor child removal: `remove_child(label)` shuts the
actor down and terminates its mailbox binding, so senders holding stale refs
fail fast instead of waiting forever. A runtime can be reduced back to zero
actors and keeps running until `shutdown()` is requested.

## Adding to a nested supervisor

`RuntimeHandle::add_actor` always targets the root supervisor. To add an actor
to an already-running nested supervisor, mint the runnable actor and its typed
ref with `Graph::dynamic_factory`, locate the nested handle, and import
`SupervisorHandleExt` to add it there:

```rust,ignore
use tokio_otp::{DynamicActorOptions, SupervisorHandleExt};

let (actor, subscription) = graph
    .dynamic_factory()
    .actor("btc-usd", Subscription::new());
let venue = handle.supervisor("coinbase").expect("venue is running");

venue
    .add_actor(actor, DynamicActorOptions::default())
    .await?;
```

The actor's label (`"btc-usd"` above) is its child id within the nested
supervisor, so remove it through the same handle with
`venue.remove_child("btc-usd")`. Actors added this way are supervised and
restart normally, but they do not appear in `RuntimeHandle::actor_stats()` or
the console; use `RuntimeHandle::add_actor` when that visibility matters.

## Name-based discovery, when you want it

When an application genuinely wants name-based discovery — plugins looking
each other up at runtime, say — build it as an ordinary actor. A typed
directory is about twenty lines:

```rust,ignore
enum DirectoryMsg<M> {
    Insert(String, ActorRef<M>),
    Get(String, Reply<Option<ActorRef<M>>>),
}

struct Directory<M> {
    entries: HashMap<String, ActorRef<M>>,
}
```

Insert refs as actors are created, and `call` the directory to resolve a name
to a typed ref. Because the directory is self-hosted, *you* choose its
semantics — namespacing, removal, versioning — instead of inheriting a
framework registry's. The runnable version is
`crates/tokio-otp/examples/directory.rs`.

Note that a directory instance is homogeneous: `Directory<M>` holds refs to
actors whose message type is that one `M`, and that is what makes lookups
fully typed with no downcasting. If several message types need discovery, run
one small directory per type. A single heterogeneous registry is possible by
storing `Box<dyn Any>` and downcasting on `Get`, but that reintroduces the
runtime type checks a stringly-typed framework registry would have imposed —
which is exactly the cost this design avoids.
