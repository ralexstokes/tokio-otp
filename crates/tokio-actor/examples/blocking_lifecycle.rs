use std::{error::Error, io, thread, time::Duration};

use tokio::sync::mpsc;
use tokio_actor::{
    ActorContext, ActorResult, BlockingOptions, BlockingTaskError, GraphBuilder, MessageHandler,
};

enum Command {
    HandleFailure,
    CancelWork,
}

#[derive(Clone)]
struct Worker {
    completed: mpsc::UnboundedSender<()>,
}

impl MessageHandler for Worker {
    type Msg = Command;

    async fn handle(&mut self, command: Command, ctx: &ActorContext<Command>) -> ActorResult {
        match command {
            Command::HandleFailure => {
                let handle = ctx.spawn_blocking(BlockingOptions::named("fallible"), |_job| {
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
                let handle = ctx.spawn_blocking(BlockingOptions::named("cancelled"), |job| {
                    loop {
                        job.checkpoint()?;
                        thread::sleep(Duration::from_millis(10));
                    }
                })?;
                handle.cancel();
                println!("cancelled blocking work: {:?}", handle.wait().await);
            }
        }
        self.completed.send(()).expect("receiver alive");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let worker = builder.add(Worker {
        completed: completed_tx,
    });
    let graph = builder.build()?;

    let handle = graph.spawn()?;

    worker.send(Command::HandleFailure).await?;
    worker.send(Command::CancelWork).await?;
    completed_rx.recv().await.expect("first command processed");
    completed_rx.recv().await.expect("second command processed");

    handle.shutdown_and_wait().await?;
    Ok(())
}
