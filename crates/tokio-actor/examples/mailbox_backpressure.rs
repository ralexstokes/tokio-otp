use std::{error::Error, sync::Arc};

use tokio::sync::Notify;
use tokio_actor::{Actor, ActorContext, ActorResult, GraphBuilder, SendError};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct ParkBeforeRecv {
    release: Arc<Notify>,
}

impl Actor for ParkBeforeRecv {
    type Msg = &'static str;

    async fn run(&self, mut ctx: ActorContext<&'static str>) -> ActorResult {
        self.release.notified().await;
        while let Some(message) = ctx.recv().await {
            println!("worker received `{message}`");
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let release = Arc::new(Notify::new());

    let mut builder = GraphBuilder::new();
    builder.name("backpressure");
    builder.mailbox_capacity(1);
    let mut worker = builder.actor(
        "worker",
        ParkBeforeRecv {
            release: release.clone(),
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
    worker.try_send("first")?;
    match worker.try_send("second") {
        Err(SendError::MailboxFull { actor_id }) => {
            println!("`{actor_id}` mailbox is full");
        }
        Ok(()) => panic!("second send unexpectedly succeeded"),
        Err(other) => panic!("unexpected send error: {other}"),
    }

    release.notify_one();
    stop.cancel();
    task.await??;

    Ok(())
}
