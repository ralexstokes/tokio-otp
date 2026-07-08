use std::error::Error;

use tokio_actor::{Actor, ActorContext, ActorResult, BlockingOptions, GraphBuilder};
use tokio_util::sync::CancellationToken;

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
    let mut worker = builder.actor("worker", Worker);
    let graph = builder.build()?;

    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    worker.wait_for_binding().await;
    worker.send("hello metrics").await?;

    stop.cancel();
    task.await??;
    Ok(())
}
