use std::error::Error;

use tokio::sync::mpsc;
use tokio_actor::{ActorContext, ActorResult, BlockingOptions, GraphBuilder, MessageHandler};

#[derive(Clone)]
struct Worker {
    completed: mpsc::UnboundedSender<()>,
}

impl MessageHandler for Worker {
    type Msg = &'static str;

    async fn handle(
        &mut self,
        message: &'static str,
        ctx: &ActorContext<&'static str>,
    ) -> ActorResult {
        tracing::info!(message, "worker received message");
        ctx.run_blocking(BlockingOptions::named("trace-blocking"), |_job| Ok(()))
            .await?;
        self.completed.send(()).expect("receiver alive");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt().init();

    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let worker = builder.add(Worker {
        completed: completed_tx,
    });
    let graph = builder.build()?;

    let handle = graph.spawn()?;

    worker.send("hello tracing").await?;
    completed_rx.recv().await.expect("message processed");

    handle.shutdown_and_wait().await?;
    Ok(())
}
