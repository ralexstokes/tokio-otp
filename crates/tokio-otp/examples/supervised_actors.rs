use std::{error::Error, time::Duration};

use tokio::{
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{Strategy, SupervisorBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "frontend",
            |mut ctx: ActorContext| async move {
                while let Some(envelope) = ctx.recv().await {
                    ctx.send("worker", envelope).await?;
                }
                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor(
            "generator",
            |ctx: ActorContext| async move {
                loop {
                    ctx.send("worker", Envelope::from_static(b"some work"))
                        .await?;

                    tokio::select! {
                        _ = ctx.shutdown_token().cancelled() => break,
                        _ = sleep(Duration::from_millis(50)) => {}
                    }
                }
                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor("worker", {
            let observed_tx = observed_tx.clone();
            move |mut ctx: ActorContext| {
                let observed_tx = observed_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        observed_tx.send(envelope).expect("receiver alive");
                    }
                    Ok(())
                }
            }
        }))
        .link("frontend", "worker")
        .link("generator", "worker")
        .ingress("requests", "frontend")
        .build()?;

    let runtime = SupervisedActors::new(graph)?
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();
    let mut ingress = handle.ingress("requests").expect("ingress exists");

    ingress.wait_for_binding().await;
    ingress.send(Envelope::from_static(b"hello")).await?;

    let _ = timeout(Duration::from_millis(300), async {
        while let Some(envelope) = observed_rx.recv().await {
            println!(
                "worker saw: {:?}",
                str::from_utf8(envelope.as_slice()).expect("valid message")
            );
        }
    })
    .await;

    handle.shutdown_and_wait().await?;
    Ok(())
}
