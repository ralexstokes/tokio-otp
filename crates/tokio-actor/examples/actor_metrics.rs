use std::error::Error;

use tokio_actor::{Actor, ActorContext, ActorResult, BlockingOptions, GraphBuilder};

#[derive(Clone)]
struct Worker;

impl Actor for Worker {
    type Msg = &'static str;

    async fn run(&self, mut ctx: ActorContext<&'static str>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            println!("processing `{message}`");
            ctx.run_blocking(BlockingOptions::named("metrics-blocking"), |_job| Ok(()))
                .await?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "metrics")]
    let _recorder = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;

    let mut builder = GraphBuilder::new();
    let worker = builder.actor("worker", Worker);
    let graph = builder.build()?;

    let handle = graph.spawn()?;

    worker.send("hello metrics").await?;

    handle.shutdown_and_wait().await?;
    Ok(())
}
