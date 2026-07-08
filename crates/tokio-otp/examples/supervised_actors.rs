use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::sync::mpsc;
use tokio_otp::prelude::*;

#[derive(Clone)]
struct Frontend {
    worker: ActorRef<String>,
}

impl Actor for Frontend {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(order) = ctx.recv().await {
            let mut worker = self.worker.clone();
            worker.send_when_ready(order).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Worker {
    runs: Arc<AtomicUsize>,
    delivered: mpsc::UnboundedSender<String>,
}

impl Actor for Worker {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        while let Some(order) = ctx.recv().await {
            if run == 0 && order.contains("jam") {
                return Err::<(), BoxError>(Box::new(io::Error::other("press jam")));
            }
            self.delivered.send(order).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (delivered_tx, mut delivered_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let worker_ref = builder.declare::<String>("worker");
    let mut orders = builder.actor("front-desk", Frontend { worker: worker_ref });
    builder.actor(
        "worker",
        Worker {
            runs: Arc::new(AtomicUsize::new(0)),
            delivered: delivered_tx,
        },
    );
    let graph = builder.build()?;

    let runtime = Runtime::builder()
        .graph(graph)
        .strategy(Strategy::OneForOne)
        .restart(Restart::Transient)
        .build()?;
    let handle = runtime.spawn();
    let mut events = handle.subscribe();

    orders.wait_for_binding().await;
    orders.send("business cards x100".to_owned()).await?;
    println!("delivered {}", delivered_rx.recv().await.expect("delivery"));

    // Crash the worker. Each run gets a fresh mailbox, so an order queued
    // behind the jam would be lost with it — wait for the supervisor to
    // restart the worker before sending more.
    orders.send("jam".to_owned()).await?;
    loop {
        let event = events.recv().await?;
        if matches!(
            &event,
            SupervisorEvent::ChildStarted { id, generation } if id == "worker" && *generation > 0
        ) {
            break;
        }
    }

    orders.send("flyers x500".to_owned()).await?;
    println!("delivered {}", delivered_rx.recv().await.expect("delivery"));

    handle.shutdown_and_wait().await?;
    Ok(())
}
