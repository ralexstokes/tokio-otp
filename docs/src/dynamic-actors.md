# Dynamic actors

The print shop is doing well, and rush orders start coming in. We don't want
to tear down the running shop to install a rush press — we want to bolt one on
while everything keeps running. [`RuntimeHandle::add_actor`] does exactly
that: it spawns a new actor as a supervised child *and* registers it so other
actors can reach it.

```rust,no_run
use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder};
use tokio_otp::{DynamicActorOptions, SupervisedActors};
use tokio_supervisor::{Strategy, SupervisorBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let graph = GraphBuilder::new()
        .name("print-shop")
        .actor(ActorSpec::from_actor(
            "front-desk",
            |mut ctx: ActorContext| async move {
                while let Some(order) = ctx.recv().await {
                    if order.as_slice().starts_with(b"rush:") {
                        // Not a build-time link: resolve `rush-press`
                        // through the runtime registry instead.
                        ctx.send_dynamic_when_ready("rush-press", order).await?;
                    } else {
                        ctx.send_when_ready("press", order).await?;
                    }
                }
                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor(
            "press",
            |mut ctx: ActorContext| async move {
                while let Some(order) = ctx.recv().await {
                    ctx.send_when_ready("shipping", order).await?;
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

    let runtime = SupervisedActors::new(graph)?
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();

    // Bolt on the rush press at runtime.
    let mut rush_press = handle
        .add_actor(
            ActorSpec::from_actor("rush-press", |mut ctx: ActorContext| async move {
                while let Some(order) = ctx.recv().await {
                    let job = String::from_utf8_lossy(order.as_slice()).into_owned();
                    let printed = format!("RUSH printed[{job}]");
                    ctx.send("shipping", Envelope::from(printed.into_bytes()))
                        .await?;
                }
                Ok(())
            }),
            DynamicActorOptions {
                // Peers this actor may `ctx.send()` to:
                peer_ids: vec!["shipping".to_owned()],
                ..DynamicActorOptions::default()
            },
        )
        .await?;
    rush_press.wait_for_binding().await;

    let mut orders = handle.ingress("orders").expect("ingress exists");
    orders.wait_for_binding().await;

    orders.send(Envelope::from_static(b"postcards x200")).await?;
    orders.send(Envelope::from_static(b"rush: wedding invites x50")).await?;

    // Anyone with the handle can message the dynamic actor directly, too:
    if let Some(direct) = handle.actor_ref("rush-press") {
        direct
            .send(Envelope::from_static(b"rush: vip banners x2"))
            .await?;
    }

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Rush season is over. Stop and unregister the actor.
    handle.remove_actor("rush-press").await?;
    assert!(handle.actor_ref("rush-press").is_none());

    handle.shutdown_and_wait().await?;
    Ok(())
}
```

Output (the two presses run concurrently, so ordering may vary):

```text
shipping: RUSH printed[rush: wedding invites x50]
shipping: RUSH printed[rush: vip banners x2]
shipping: postcards x200
```

## How it fits together

**[`DynamicActorOptions`]** carries the supervision policies for the new child
(`restart`, `shutdown`, `restart_intensity` — same meanings as ever, with
`Restart::Transient` as the default) plus `peer_ids`, the list of existing
actors the dynamic actor is allowed to `ctx.send()` to. Think of `peer_ids` as
the runtime version of `GraphBuilder::link`.

**`add_actor` returns an [`ActorRef`]** bound to the new actor's mailbox — the
same stable-handle kind as everything else, so it survives restarts of the
dynamic actor. `handle.actor_ref(id)` looks up the same reference later.

**`send_dynamic` / `send_dynamic_when_ready`** are how *static* actors reach
*dynamic* ones. Static links are validated at build time, but a dynamic actor
does not exist yet when the graph is built, so these variants resolve the
target through the runtime's [`ActorRegistry`] instead. Sending to an id that
is not registered at all is an error; the `_when_ready` flavor's job is to
retry across *restart windows* of an already-registered target, just like
`send_when_ready` does for static links. (That is why the example installs
the rush press before opening the ingress for orders.)

**`remove_actor`** shuts the child down with its shutdown policy and removes
it from the registry, after which dynamic sends to that id fail again.

Because a dynamic actor is just a supervised child, everything from the
previous chapters applies: if the rush press jams, it restarts under its own
policies, and its `ActorRef` keeps working.

[`RuntimeHandle::add_actor`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-otp
[`DynamicActorOptions`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-otp
[`ActorRef`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
[`ActorRegistry`]: https://github.com/ralexstokes/tokio-otp/tree/main/crates/tokio-actor
