use std::error::Error;

use tokio_actor::{Actor, ActorContext, ActorResult, BlockingOptions, GraphBuilder};
use tokio_util::sync::CancellationToken;

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
    let mut worker = builder.actor("worker", Worker);
    let graph = builder.build()?;

    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    worker.wait_for_binding().await;
    worker.send("hello tracing").await?;

    stop.cancel();
    task.await??;
    Ok(())
}
