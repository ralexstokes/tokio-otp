use std::{error::Error, io, thread, time::Duration};

use tokio::{sync::oneshot, time::sleep};
use tokio_actor::{ActorContext, ActorSpec, BlockingOptions, BlockingTaskError, GraphBuilder};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (done_tx, done_rx) = oneshot::channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let done_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(done_tx)));
            move |mut ctx: ActorContext| {
                let done_tx = std::sync::Arc::clone(&done_tx);
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        match envelope.as_slice() {
                            b"handle failure locally" => {
                                match ctx
                                    .run_blocking(BlockingOptions::named("parse-job"), |_job| {
                                        Err(io::Error::other("invalid payload").into())
                                    })
                                    .await
                                {
                                    Err(BlockingTaskError::Failed(error)) => {
                                        println!("run_blocking handled locally: {error}");
                                    }
                                    Ok(()) => panic!("blocking job unexpectedly succeeded"),
                                    Err(other) => panic!("unexpected blocking error: {other}"),
                                }
                            }
                            b"cancel blocking task" => {
                                let handle = ctx.spawn_blocking(
                                    BlockingOptions::named("cancellable-job"),
                                    |job| {
                                        loop {
                                            job.checkpoint()?;
                                            thread::sleep(Duration::from_millis(20));
                                        }
                                    },
                                )?;

                                println!(
                                    "spawned blocking task {} named {:?}",
                                    handle.id(),
                                    handle.name()
                                );

                                sleep(Duration::from_millis(60)).await;
                                handle.cancel();

                                match handle.wait().await {
                                    Err(BlockingTaskError::Cancelled) => {
                                        println!("spawn_blocking cancellation observed");
                                    }
                                    Ok(()) => panic!("blocking task unexpectedly completed"),
                                    Err(other) => panic!("unexpected blocking error: {other}"),
                                }

                                if let Some(tx) = done_tx.lock().expect("mutex not poisoned").take()
                                {
                                    let _ = tx.send(());
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                }
            }
        }))
        .ingress("requests", "worker")
        .build()?;

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    ingress.wait_for_binding().await;
    ingress.send("handle failure locally").await?;
    ingress.send("cancel blocking task").await?;

    done_rx.await.expect("worker finished the lifecycle demo");

    stop.cancel();
    task.await??;
    Ok(())
}
