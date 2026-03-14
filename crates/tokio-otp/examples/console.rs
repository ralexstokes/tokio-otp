use std::{
    error::Error,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::time::sleep;
use tokio_actor::{ActorContext, ActorSpec, Envelope, GraphBuilder};
use tokio_otp::SupervisedActors;
use tokio_supervisor::{RestartIntensity, Strategy, SupervisorBuilder};

const MESSAGE_COUNT: usize = 30;
const FAILURE_INTERVAL: usize = 5;

fn actor_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync + 'static> {
    Box::new(io::Error::other(message.into()))
}

fn payload_for(message_index: usize) -> String {
    if message_index.is_multiple_of(FAILURE_INTERVAL) {
        format!("fail-worker-{message_index}")
    } else {
        format!("job-{message_index}")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let worker_runs = Arc::new(AtomicUsize::new(0));

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "frontend",
            |mut ctx: ActorContext| async move {
                while let Some(envelope) = ctx.recv().await {
                    ctx.send_when_ready("worker", envelope).await?;
                }
                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor("worker", {
            let worker_runs = Arc::clone(&worker_runs);
            move |mut ctx: ActorContext| {
                let worker_runs = Arc::clone(&worker_runs);
                async move {
                    let run = worker_runs.fetch_add(1, Ordering::SeqCst) + 1;
                    println!("worker generation {run} started");

                    while let Some(envelope) = ctx.recv().await {
                        let payload = String::from_utf8_lossy(envelope.as_slice()).into_owned();
                        println!("worker generation {run} received `{payload}`");

                        if payload.starts_with("fail-worker") {
                            println!("worker generation {run} failing on `{payload}`");
                            return Err(actor_error(format!(
                                "simulated worker failure triggered by `{payload}`"
                            )));
                        }

                        sleep(Duration::from_millis(250)).await;
                    }

                    println!("worker generation {run} stopped");
                    Ok(())
                }
            }
        }))
        .link("frontend", "worker")
        .ingress("requests", "frontend")
        .build()?;

    let runtime = SupervisedActors::new(graph)?
        .actor_restart_intensity(
            "worker",
            RestartIntensity::new(20, Duration::from_secs(120)),
        )
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))?;
    let handle = runtime.spawn();

    let console = handle
        .console()
        .bind(([127, 0, 0, 1], 0))
        .build()
        .spawn()
        .await?;

    println!("console available at http://{}", console.local_addr());
    println!("watch the worker restart every {FAILURE_INTERVAL}th message");

    let mut ingress = handle.ingress("requests").expect("ingress exists");
    ingress.wait_for_binding().await;

    for message_index in 1..=MESSAGE_COUNT {
        sleep(Duration::from_secs(1)).await;
        let payload = payload_for(message_index);

        println!("sending `{payload}`");
        ingress.send(Envelope::from(payload.into_bytes())).await?;
    }

    println!("shutting down runtime");
    handle.shutdown_and_wait().await?;
    console.shutdown();
    Ok(())
}
