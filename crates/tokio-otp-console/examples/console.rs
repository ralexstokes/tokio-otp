//! Demo workload for the web console.
//!
//! Builds a supervision tree with nested supervisors and keeps it busy so the
//! console has something to show:
//!
//! ```text
//! root (OneForOne)
//! ├── frontend            graph actor, forwards jobs to the worker
//! ├── worker              graph actor, fails every 6th job (backoff 1.5s)
//! ├── burst               dynamic actor, added and removed on a cycle
//! ├── pipeline (OneForAll)
//! │   ├── source          runs until shutdown
//! │   └── transform       fails every 9s (backoff 2s), bouncing its sibling
//! └── telemetry (OneForOne)
//!     ├── heartbeat       completes cleanly every 7s, RestartPolicy::Always
//!     ├── schema-migration completes once, RestartPolicy::Never
//!     └── exporters (OneForOne)
//!         └── stdout-exporter  runs until shutdown
//! ```
//!
//! Run with `cargo run -p tokio-otp-console --example console`,
//! open the printed URL, and stop with Ctrl-C.

use std::{error::Error, io, time::Duration};

use tokio::time::sleep;
use tokio_otp::{
    Actor, ActorContext, ActorRef, ActorResult, BoxError, DynamicActorOptions, GraphBuilder,
    SupervisedActors,
};
use tokio_otp_console::Console;
use tokio_supervisor::{
    BackoffPolicy, ChildSpec, RestartIntensity, RestartPolicy, Strategy, Supervisor,
    SupervisorBuilder,
};

fn example_error(message: &'static str) -> BoxError {
    Box::new(io::Error::other(message))
}

#[derive(Clone)]
struct Frontend {
    worker: ActorRef<String>,
}

impl Actor for Frontend {
    type Msg = String;

    async fn handle(&mut self, message: String, _ctx: &ActorContext<String>) -> ActorResult {
        self.worker.send(message).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct Worker;

impl Actor for Worker {
    type Msg = String;

    async fn handle(&mut self, message: String, _ctx: &ActorContext<String>) -> ActorResult {
        if message.contains("fail") {
            println!("worker failing on `{message}`");
            return Err::<(), BoxError>(example_error("simulated worker failure"));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Burst;

impl Actor for Burst {
    type Msg = u32;

    async fn handle(&mut self, _message: u32, _ctx: &ActorContext<u32>) -> ActorResult {
        Ok(())
    }
}

/// A OneForAll group: when `transform` fails, `source` is stopped and
/// restarted along with it.
fn pipeline_supervisor() -> Result<Supervisor, Box<dyn Error>> {
    let source = ChildSpec::new("source", |ctx| async move {
        ctx.shutdown_token().cancelled().await;
        Ok(())
    });

    let transform = ChildSpec::new("transform", |ctx| async move {
        tokio::select! {
            _ = ctx.shutdown_token().cancelled() => Ok(()),
            _ = sleep(Duration::from_secs(9)) => {
                println!("transform failing (generation {})", ctx.generation());
                Err(example_error("pipeline transform wedged"))
            }
        }
    })
    .restart_intensity(
        RestartIntensity::new(60, Duration::from_secs(60))
            .with_backoff(BackoffPolicy::Fixed(Duration::from_secs(2))),
    );

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .restart_intensity(RestartIntensity::new(60, Duration::from_secs(60)))
        .child(source)
        .child(transform)
        .build()?;
    Ok(supervisor)
}

/// A OneForOne group demonstrating the other restart policies, with a further
/// nested supervisor one level deeper.
fn telemetry_supervisor() -> Result<Supervisor, Box<dyn Error>> {
    // `Always` restarts even after a clean exit, so this child completes and
    // comes back every 7 seconds.
    let heartbeat = ChildSpec::new("heartbeat", |ctx| async move {
        tokio::select! {
            _ = ctx.shutdown_token().cancelled() => {}
            _ = sleep(Duration::from_secs(7)) => {}
        }
        Ok(())
    })
    .restart(RestartPolicy::Always);

    // `Never` runs at most once: this child completes after 3 seconds and
    // stays Stopped for the rest of the run.
    let migration = ChildSpec::new("schema-migration", |_ctx| async move {
        sleep(Duration::from_secs(3)).await;
        Ok(())
    })
    .restart(RestartPolicy::Never);

    let exporter = ChildSpec::new("stdout-exporter", |ctx| async move {
        ctx.shutdown_token().cancelled().await;
        Ok(())
    });
    let exporters = SupervisorBuilder::new().child(exporter).build()?;

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForOne)
        .restart_intensity(RestartIntensity::new(60, Duration::from_secs(60)))
        .child(heartbeat)
        .child(migration)
        .supervisor("exporters", exporters)
        .build()?;
    Ok(supervisor)
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
    builder.define(worker_slot, Worker);
    let graph = builder.build()?;

    let root = SupervisorBuilder::new()
        .strategy(Strategy::OneForOne)
        .restart_intensity(RestartIntensity::new(60, Duration::from_secs(60)))
        .supervisor("pipeline", pipeline_supervisor()?)
        .supervisor("telemetry", telemetry_supervisor()?);

    let runtime = SupervisedActors::new(graph)
        .actor_restart_intensity(
            &worker_ref,
            RestartIntensity::new(60, Duration::from_secs(60))
                .with_backoff(BackoffPolicy::Fixed(Duration::from_millis(1500))),
        )
        .build_runtime(root)?;
    let handle = runtime.spawn();

    let console = match Console::for_runtime(&handle)
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

    // Periodically add a dynamic actor, feed it, and remove it again, so the
    // console shows children joining and leaving the tree.
    let dynamic = handle.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(12)).await;
            let Ok(burst) = dynamic
                .add_actor("burst", Burst, DynamicActorOptions::default())
                .await
            else {
                break;
            };
            for index in 0..40u32 {
                if burst.send(index).await.is_err() {
                    break;
                }
                sleep(Duration::from_millis(150)).await;
            }
            sleep(Duration::from_secs(4)).await;
            if dynamic.remove_child("burst").await.is_err() {
                break;
            }
        }
    });

    println!("press Ctrl-C to stop");

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let mut message_index = 0usize;
    loop {
        tokio::select! {
            _ = &mut ctrl_c => break,
            _ = sleep(Duration::from_millis(800)) => {
                message_index += 1;
                let payload = if message_index.is_multiple_of(6) {
                    format!("fail-worker-{message_index}")
                } else {
                    format!("job-{message_index}")
                };
                frontend.send(payload).await?;
            }
        }
    }

    println!("shutting down");
    handle.shutdown_and_wait().await?;
    if let Some(console) = console {
        console.shutdown();
    }
    Ok(())
}
