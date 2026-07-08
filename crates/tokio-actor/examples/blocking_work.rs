use std::error::Error;

use tokio::sync::mpsc;
use tokio_actor::{ActorContext, ActorResult, BlockingOptions, GraphBuilder, MessageHandler};

enum WorkMsg {
    Process(String),
    Finished(String),
}

#[derive(Clone)]
struct Worker {
    observed: mpsc::UnboundedSender<String>,
}

impl MessageHandler for Worker {
    type Msg = WorkMsg;

    async fn handle(&mut self, message: WorkMsg, ctx: &ActorContext<WorkMsg>) -> ActorResult {
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
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let worker = builder.actor(
        "worker",
        Worker {
            observed: observed_tx,
        },
    );
    let graph = builder.build()?;

    let handle = graph.spawn()?;

    worker
        .send(WorkMsg::Process("hello blocking actor".to_owned()))
        .await?;
    println!("result: {}", observed_rx.recv().await.expect("result"));

    handle.shutdown_and_wait().await?;
    Ok(())
}
