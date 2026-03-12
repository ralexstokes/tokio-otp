use std::{error::Error, time::Duration};

use tokio::{
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder};
use tokio_supervisor::{ChildSpec, SupervisorBuilder};

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
                    sleep(Duration::from_millis(50)).await;
                }
            },
        ))
        .actor(ActorSpec::from_actor("worker", {
            let observed_tx = observed_tx;
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

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let supervised_graph = ChildSpec::new("actor-graph", {
        let graph = graph;
        move |ctx| {
            let graph = graph.clone();
            async move {
                graph
                    .run_until(ctx.token.cancelled())
                    .await
                    .map_err(Into::into)
            }
        }
    });

    let supervisor = SupervisorBuilder::new().child(supervised_graph).build()?;
    let handle = supervisor.spawn();

    ingress.wait_for_binding().await;
    ingress.send(Envelope::from_static(b"hello")).await?;

    // observe the stream of work for some time...
    let _ = timeout(Duration::from_millis(300), async {
        while let Some(envelope) = observed_rx.recv().await {
            println!(
                "worker saw: {:?}",
                str::from_utf8(envelope.as_slice()).expect("some message")
            );
        }
    })
    .await;

    handle.shutdown_and_wait().await?;
    Ok(())
}
