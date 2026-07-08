use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio_actor::{ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder, MessageHandler};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{RestartIntensity, Strategy, SupervisorBuilder, SupervisorEvent};

#[derive(Clone)]
struct Frontend {
    worker: ActorRef<String>,
}

impl MessageHandler for Frontend {
    type Msg = String;

    async fn handle(&mut self, message: String, _ctx: &ActorContext<String>) -> ActorResult {
        let mut worker = self.worker.clone();
        worker.send_when_ready(message).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct Worker {
    runs: Arc<AtomicUsize>,
    run: usize,
}

impl MessageHandler for Worker {
    type Msg = String;

    async fn on_start(&mut self, _ctx: &ActorContext<String>) -> ActorResult {
        self.run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(())
    }

    async fn handle(&mut self, message: String, _ctx: &ActorContext<String>) -> ActorResult {
        println!("worker generation {} received `{message}`", self.run);
        if message == "fail-worker" {
            return Err::<(), BoxError>(Box::new(io::Error::other("simulated failure")));
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
            run: 0,
        },
    );
    let graph = builder.build()?;

    let runtime = SupervisedActors::new(graph)?
        .actor_restart_intensity("worker", RestartIntensity::new(2, Duration::from_secs(1)))
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();
    let mut events = handle.subscribe();
    let mut restart_events = handle.subscribe();
    let mut snapshots = handle.subscribe_snapshots();

    let event_task = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            println!("event: {event:?}");
        }
    });

    frontend.wait_for_binding().await;
    frontend.send("hello".to_owned()).await?;
    frontend.send("fail-worker".to_owned()).await?;
    loop {
        let event = restart_events.recv().await?;
        if matches!(
            &event,
            SupervisorEvent::ChildStarted { id, generation } if id == "worker" && *generation > 0
        ) {
            break;
        }
    }
    frontend.send("after-restart".to_owned()).await?;

    snapshots.changed().await?;
    println!("snapshot: {:?}", snapshots.borrow().state);

    handle.shutdown_and_wait().await?;
    event_task.abort();
    Ok(())
}
