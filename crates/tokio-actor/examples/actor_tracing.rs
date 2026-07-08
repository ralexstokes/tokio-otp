use std::error::Error;

use tokio_actor::{Actor, ActorContext, ActorResult, BlockingOptions, GraphBuilder};

#[derive(Clone)]
struct Worker;

impl Actor for Worker {
    type Msg = &'static str;

    async fn run(&self, mut ctx: ActorContext<&'static str>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            tracing::info!(message, "worker received message");
            ctx.run_blocking(BlockingOptions::named("trace-blocking"), |_job| Ok(()))
                .await?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt().init();

    let mut builder = GraphBuilder::new();
    let worker = builder.actor("worker", Worker);
    let graph = builder.build()?;

    let handle = graph.spawn()?;

    worker.send("hello tracing").await?;

    handle.shutdown_and_wait().await?;
    Ok(())
}
