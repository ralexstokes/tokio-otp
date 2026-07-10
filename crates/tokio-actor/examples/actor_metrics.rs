use std::error::Error;

use tokio::sync::mpsc;
use tokio_actor::{Actor, ActorContext, ActorResult, GraphBuilder};

#[derive(Clone)]
struct Worker {
    completed: mpsc::UnboundedSender<()>,
}

impl Actor for Worker {
    type Msg = &'static str;

    async fn handle(
        &mut self,
        message: &'static str,
        ctx: &ActorContext<&'static str>,
    ) -> ActorResult {
        println!("processing `{message}`");
        ctx.run_blocking(|_token| ()).await;
        self.completed.send(()).expect("receiver alive");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "metrics")]
    let _recorder = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;

    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let worker = builder.add(Worker {
        completed: completed_tx,
    });
    let graph = builder.build()?;

    let handle = graph.spawn()?;

    worker.send("hello metrics").await?;
    completed_rx.recv().await.expect("message processed");

    handle.shutdown_and_wait().await?;
    Ok(())
}
