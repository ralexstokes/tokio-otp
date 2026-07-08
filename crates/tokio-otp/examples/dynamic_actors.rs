use std::{error::Error, marker::PhantomData};

use tokio::sync::mpsc;
use tokio_actor::{Actor, ActorContext, ActorResult, GraphBuilder};
use tokio_otp::{DynamicActorOptions, SupervisedActors};
use tokio_supervisor::{Strategy, SupervisorBuilder};

struct Drain<M>(PhantomData<fn(M)>);

impl<M> Drain<M> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<M> Clone for Drain<M> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<M: Send + 'static> Actor for Drain<M> {
    type Msg = M;

    async fn run(&self, mut ctx: ActorContext<M>) -> ActorResult {
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[derive(Clone)]
struct Frontend;

impl Actor for Frontend {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(order) = ctx.recv().await {
            let mut rush = ctx
                .registry()
                .expect("registry installed")
                .actor_ref::<String>("rush-press")?;
            rush.send_when_ready(order).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RushPress {
    observed: mpsc::UnboundedSender<String>,
}

impl Actor for RushPress {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(order) = ctx.recv().await {
            self.observed.send(order).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let mut orders = builder.actor("front-desk", Frontend);
    builder.actor("seed", Drain::<()>::new());
    let graph = builder.build()?;

    let runtime = SupervisedActors::new(graph)?
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();

    let mut rush = handle
        .add_actor(
            "rush-press",
            RushPress {
                observed: observed_tx,
            },
            DynamicActorOptions::default(),
        )
        .await?;

    orders.wait_for_binding().await;
    rush.wait_for_binding().await;
    orders.send("wedding invites x50".to_owned()).await?;
    rush.send("vip banners x2".to_owned()).await?;

    for _ in 0..2 {
        println!("rush job {}", observed_rx.recv().await.expect("rush job"));
    }

    handle.remove_actor("rush-press").await?;
    handle.shutdown_and_wait().await?;
    Ok(())
}
