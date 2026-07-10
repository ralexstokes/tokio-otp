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

const MESSAGE_COUNT: usize = 10;

#[derive(Clone)]
struct Frontend {
    worker: ActorRef<String>,
}

impl Actor for Frontend {
    type Msg = String;

    async fn handle(&mut self, message: String, _ctx: &ActorContext<String>) -> ActorResult {
        let worker = self.worker.clone();
        worker.send(message).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct Worker {
    runs: Arc<AtomicUsize>,
    run: usize,
}

impl Actor for Worker {
    type Msg = String;

    async fn on_start(&mut self, _ctx: &ActorContext<String>) -> ActorResult {
        self.run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(())
    }

    async fn handle(&mut self, message: String, _ctx: &ActorContext<String>) -> ActorResult {
        println!("worker generation {} received `{message}`", self.run);
        if message.contains("fail") {
            return Err::<(), BoxError>(Box::new(io::Error::other("simulated failure")));
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut builder = GraphBuilder::new();
    let (worker_slot, worker_ref) = builder.slot::<String>("worker");
    let frontend = builder.actor(
        "frontend",
        Frontend {
            worker: worker_ref.clone(),
        },
    );
    builder.define(
        worker_slot,
        Worker {
            runs: Arc::new(AtomicUsize::new(0)),
            run: 0,
        },
    );
    let graph = builder.build()?;

    let runtime = SupervisedActors::new(graph)
        .actor_restart_intensity(
            &worker_ref,
            RestartIntensity::new(20, Duration::from_secs(120)),
        )
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();

    let console = match handle
        .console()
        .bind(([127, 0, 0, 1], 0))
        .build()
        .spawn()
        .await
    {
        Ok(console) => {
            println!("console available at http://{}", console.local_addr());
            Some(console)
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            println!("console bind skipped: {error}");
            None
        }
        Err(error) => return Err(error.into()),
    };

    for message_index in 1..=MESSAGE_COUNT {
        let payload = if message_index % 5 == 0 {
            format!("fail-worker-{message_index}")
        } else {
            format!("job-{message_index}")
        };
        frontend.send(payload).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    handle.shutdown_and_wait().await?;
    if let Some(console) = console {
        console.shutdown();
    }
    Ok(())
}
