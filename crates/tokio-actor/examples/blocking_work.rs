use std::error::Error;

use tokio::sync::mpsc;
use tokio_actor::{Actor, ActorContext, ActorResult, BlockingOptions, GraphBuilder};
use tokio_util::sync::CancellationToken;

enum WorkMsg {
    Process(String),
    Finished(String),
}

#[derive(Clone)]
struct Worker {
    observed: mpsc::UnboundedSender<String>,
}

impl Actor for Worker {
    type Msg = WorkMsg;

    async fn run(&self, mut ctx: ActorContext<WorkMsg>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            match message {
                WorkMsg::Process(input) => {
                    ctx.run_blocking(BlockingOptions::named("uppercase"), move |job| {
                        let output = input.to_uppercase();
                        job.myself().blocking_send(WorkMsg::Finished(output))?;
                        Ok(())
                    })
                    .await?;
                }
                WorkMsg::Finished(output) => {
                    self.observed.send(output).expect("receiver alive");
                }
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let mut worker = builder.actor(
        "worker",
        Worker {
            observed: observed_tx,
        },
    );
    let graph = builder.build()?;

    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    worker.wait_for_binding().await;
    worker
        .send(WorkMsg::Process("hello blocking actor".to_owned()))
        .await?;
    println!("result: {}", observed_rx.recv().await.expect("result"));

    stop.cancel();
    task.await??;
    Ok(())
}
