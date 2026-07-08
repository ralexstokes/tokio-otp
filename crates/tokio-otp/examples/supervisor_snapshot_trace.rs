use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio_actor::{Actor, ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{RestartIntensity, Strategy, SupervisorBuilder};

#[derive(Clone)]
struct Frontend {
    worker: ActorRef<String>,
}

impl Actor for Frontend {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            let mut worker = self.worker.clone();
            worker.send_when_ready(message).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Worker {
    runs: Arc<AtomicUsize>,
}

impl Actor for Worker {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        while let Some(message) = ctx.recv().await {
            println!("worker generation {run} received `{message}`");
            if message == "fail-worker" {
                return Err::<(), BoxError>(Box::new(io::Error::other("simulated failure")));
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut builder = GraphBuilder::new();
    let worker_ref = builder.declare::<String>("worker");
    let mut frontend = builder.actor("frontend", Frontend { worker: worker_ref });
    builder.actor(
        "worker",
        Worker {
            runs: Arc::new(AtomicUsize::new(0)),
        },
    );
    let graph = builder.build()?;

    let runtime = SupervisedActors::new(graph)?
        .actor_restart_intensity("worker", RestartIntensity::new(2, Duration::from_secs(1)))
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();
    let mut events = handle.subscribe();
    let mut snapshots = handle.subscribe_snapshots();

    let event_task = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            println!("event: {event:?}");
        }
    });

    frontend.wait_for_binding().await;
    frontend.send("hello".to_owned()).await?;
    frontend.send("fail-worker".to_owned()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    frontend.send("after-restart".to_owned()).await?;

    snapshots.changed().await?;
    println!("snapshot: {:?}", snapshots.borrow().state);

    handle.shutdown_and_wait().await?;
    event_task.abort();
    Ok(())
}
