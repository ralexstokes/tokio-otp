use std::{error::Error, thread, time::Duration};

use tokio::sync::mpsc;
use tokio_actor::{
    Actor, ActorContext, ActorResult, BlockingOptions, GraphBuilder, SpawnBlockingError,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Worker {
    observed: mpsc::UnboundedSender<String>,
}

impl Actor for Worker {
    type Msg = ();

    async fn run(&self, mut ctx: ActorContext<()>) -> ActorResult {
        while ctx.recv().await.is_some() {
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
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    builder.max_blocking_tasks_per_actor(1);
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
    worker.send(()).await?;
    println!("{}", observed_rx.recv().await.expect("capacity result"));

    stop.cancel();
    task.await??;
    Ok(())
}
