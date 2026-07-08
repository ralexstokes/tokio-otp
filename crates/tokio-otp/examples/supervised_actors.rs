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

impl MessageHandler for Frontend {
    type Msg = String;

    async fn handle(&mut self, order: String, _ctx: &ActorContext<String>) -> ActorResult {
        let worker = self.worker.clone();
        worker.send(order).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct Worker {
    runs: Arc<AtomicUsize>,
    delivered: mpsc::UnboundedSender<String>,
    run: usize,
}

impl MessageHandler for Worker {
    type Msg = String;

    async fn on_start(&mut self, _ctx: &ActorContext<String>) -> ActorResult {
        self.run = self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, order: String, _ctx: &ActorContext<String>) -> ActorResult {
        if self.run == 0 && order.contains("jam") {
            return Err::<(), BoxError>(Box::new(io::Error::other("press jam")));
        }
        self.delivered.send(order).expect("receiver alive");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (delivered_tx, mut delivered_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let worker_ref = builder.declare::<String>("worker");
    let orders = builder.actor("front-desk", Frontend { worker: worker_ref });
    builder.actor(
        "worker",
        Worker {
            runs: Arc::new(AtomicUsize::new(0)),
            delivered: delivered_tx,
            run: 0,
        },
    );
    let graph = builder.build()?;

    let runtime = Runtime::builder()
        .graph(graph)
        .strategy(Strategy::OneForOne)
        .restart(Restart::Transient)
        .build()?;
    let handle = runtime.spawn();

    orders.send("business cards x100".to_owned()).await?;
    println!("delivered {}", delivered_rx.recv().await.expect("delivery"));

    // Crash the worker. Each run gets a fresh mailbox, so an order queued
    // behind the jam would be lost with it — wait for the supervisor to
    // restart the worker before sending more.
    let restart = handle.monitor_restart("worker")?;
    orders.send("jam".to_owned()).await?;
    restart.await?;

    orders.send("flyers x500".to_owned()).await?;
    println!("delivered {}", delivered_rx.recv().await.expect("delivery"));

    handle.shutdown_and_wait().await?;
    Ok(())
}
