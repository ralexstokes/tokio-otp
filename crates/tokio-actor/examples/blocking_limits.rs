use std::{error::Error, thread, time::Duration};

use tokio::sync::mpsc;
use tokio_actor::{
    ActorContext, ActorResult, BlockingOptions, GraphBuilder, MessageHandler, SpawnBlockingError,
};

#[derive(Clone)]
struct Worker {
    observed: mpsc::UnboundedSender<String>,
}

impl MessageHandler for Worker {
    type Msg = ();

    async fn handle(&mut self, _message: (), ctx: &ActorContext<()>) -> ActorResult {
        let _first = ctx.spawn_blocking(BlockingOptions::named("held"), |_job| {
            thread::sleep(Duration::from_millis(250));
            Ok(())
        })?;
        match ctx.spawn_blocking(BlockingOptions::named("rejected"), |_job| Ok(())) {
            Err(SpawnBlockingError::AtCapacity { actor_id, .. }) => {
                self.observed
                    .send(format!("`{actor_id}` is at blocking capacity"))
                    .expect("receiver alive");
            }
            other => self
                .observed
                .send(format!("unexpected result: {other:?}"))
                .expect("receiver alive"),
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    builder.max_blocking_tasks_per_actor(1);
    let worker = builder.add(Worker {
        observed: observed_tx,
    });
    let graph = builder.build()?;

    let handle = graph.spawn()?;

    worker.send(()).await?;
    println!("{}", observed_rx.recv().await.expect("capacity result"));

    handle.shutdown_and_wait().await?;
    Ok(())
}
