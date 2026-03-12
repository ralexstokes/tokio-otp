use std::{error::Error, thread, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{
    Actor, ActorContext, ActorResult, ActorSpec, BlockingOptions, Envelope, GraphBuilder,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Dispatcher;

impl Actor for Dispatcher {
    async fn run(&self, mut ctx: ActorContext) -> ActorResult {
        let sink = ctx.peer("sink").expect("linked sink exists");

        while let Some(envelope) = ctx.recv().await {
            let sink = sink.clone();
            let _background =
                ctx.spawn_blocking(BlockingOptions::named("uppercase"), move |job| {
                    // emulate heavy work...
                    for _ in 0..5 {
                        job.checkpoint()?;
                        thread::sleep(Duration::from_millis(20));
                    }

                    let uppercased: Vec<u8> = envelope
                        .as_slice()
                        .iter()
                        .map(|byte| byte.to_ascii_uppercase())
                        .collect();
                    sink.blocking_send(uppercased)?;
                    Ok(())
                })?;
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("dispatcher", Dispatcher))
        .actor(ActorSpec::from_actor("sink", {
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
        .link("dispatcher", "sink")
        .ingress("requests", "dispatcher")
        .build()?;

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"hello blocking actor"))
        .await?;
    let envelope = timeout(Duration::from_secs(2), observed_rx.recv())
        .await?
        .expect("blocking result received");
    println!(
        "blocking result: {}",
        std::str::from_utf8(envelope.as_slice())?
    );

    stop.cancel();
    task.await??;
    Ok(())
}
