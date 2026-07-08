use std::{error::Error, io, thread, time::Duration};

use tokio_actor::{
    Actor, ActorContext, ActorResult, BlockingOptions, BlockingTaskError, GraphBuilder,
};
use tokio_util::sync::CancellationToken;

enum Command {
    HandleFailure,
    CancelWork,
}

#[derive(Clone)]
struct Worker;

impl Actor for Worker {
    type Msg = Command;

    async fn run(&self, mut ctx: ActorContext<Command>) -> ActorResult {
        while let Some(command) = ctx.recv().await {
            match command {
                Command::HandleFailure => {
                    let handle = ctx
                        .spawn_blocking(BlockingOptions::named("fallible"), |_job| {
                            Err(io::Error::other("boom").into())
                        })?;
                    match handle.wait().await {
                        Err(BlockingTaskError::Failed(error)) => {
                            println!("handled blocking failure: {error}");
                        }
                        other => println!("unexpected blocking result: {other:?}"),
                    }
                }
                Command::CancelWork => {
                    let handle =
                        ctx.spawn_blocking(BlockingOptions::named("cancelled"), |job| {
                            loop {
                                job.checkpoint()?;
                                thread::sleep(Duration::from_millis(10));
                            }
                        })?;
                    handle.cancel();
                    println!("cancelled blocking work: {:?}", handle.wait().await);
                }
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut builder = GraphBuilder::new();
    let mut worker = builder.actor("worker", Worker);
    let graph = builder.build()?;

    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    worker.wait_for_binding().await;
    worker.send(Command::HandleFailure).await?;
    worker.send(Command::CancelWork).await?;

    stop.cancel();
    task.await??;
    Ok(())
}
