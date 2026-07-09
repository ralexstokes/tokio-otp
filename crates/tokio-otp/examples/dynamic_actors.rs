use std::error::Error;

use tokio::sync::mpsc;
use tokio_actor::{Actor, ActorContext, ActorResult};
use tokio_otp::{DynamicActorOptions, Runtime};
use tokio_supervisor::{Strategy, SupervisorExit};

#[derive(Clone)]
struct Frontend;

impl Actor for Frontend {
    type Msg = String;

    async fn handle(&mut self, order: String, ctx: &ActorContext<String>) -> ActorResult {
        let rush = ctx
            .registry()
            .expect("registry installed")
            .actor_ref::<String>("rush-press")?;
        rush.send(order).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct RushPress {
    observed: mpsc::UnboundedSender<String>,
}

impl Actor for RushPress {
    type Msg = String;

    async fn handle(&mut self, order: String, _ctx: &ActorContext<String>) -> ActorResult {
        self.observed.send(order).expect("receiver alive");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let runtime = Runtime::builder()
        .dynamic()
        .strategy(Strategy::OneForOne)
        .build()?;
    let handle = runtime.spawn();

    let orders = handle
        .add_actor("front-desk", Frontend, DynamicActorOptions::default())
        .await?;
    let rush = handle
        .add_actor(
            "rush-press",
            RushPress {
                observed: observed_tx,
            },
            DynamicActorOptions::default(),
        )
        .await?;

    orders.send("wedding invites x50".to_owned()).await?;
    let observed = observed_rx.recv().await.expect("rush job");
    assert_eq!(observed, "wedding invites x50");
    println!("rush job {observed}");

    rush.send("vip banners x2".to_owned()).await?;
    let observed = observed_rx.recv().await.expect("rush job");
    assert_eq!(observed, "vip banners x2");
    println!("rush job {observed}");

    handle.remove_actor("front-desk").await?;
    handle.remove_actor("rush-press").await?;
    let exit = handle.shutdown_and_wait().await?;
    assert_eq!(exit, SupervisorExit::Completed);
    Ok(())
}
