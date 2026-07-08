use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::sync::mpsc;
use tokio_actor::{Actor, ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{Restart, RestartIntensity, Strategy, SupervisorBuilder};

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
    observed: mpsc::UnboundedSender<String>,
}

impl Actor for Worker {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        while let Some(order) = ctx.recv().await {
            if run == 0 && order == "fail-worker" {
                return Err::<(), BoxError>(Box::new(io::Error::other("worker failed")));
            }
            self.observed.send(order).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let worker_ref = builder.declare::<String>("worker");
    let mut frontend = builder.actor("frontend", Frontend { worker: worker_ref });
    builder.actor(
        "worker",
        Worker {
            runs: Arc::new(AtomicUsize::new(0)),
            observed: observed_tx,
        },
    );
    let graph = builder.build()?;

    let children = SupervisedActors::new(graph)?
        .actor_restart("worker", Restart::Transient)
        .actor_restart_intensity(
            "worker",
            RestartIntensity::new(5, std::time::Duration::from_secs(5)),
        )
        .build()?;
    let supervisor = children
        .into_iter()
        .fold(
            SupervisorBuilder::new().strategy(Strategy::OneForOne),
            |builder, child| builder.child(child),
        )
        .build()?;
    let handle = supervisor.spawn();

    frontend.wait_for_binding().await;
    frontend.send("fail-worker".to_owned()).await?;
    frontend.send("after-restart".to_owned()).await?;
    println!("observed {}", observed_rx.recv().await.expect("message"));

    handle.shutdown_and_wait().await?;
    Ok(())
}
