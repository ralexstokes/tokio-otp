use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use tokio::{
    sync::{Notify, mpsc},
    time::timeout,
};
use tokio_actor::{
    ActorContext, ActorSpec, BlockingOptions, BlockingTaskError, GraphBuilder, SpawnBlockingError,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let blocking_started = Arc::new(Notify::new());
    let stuck_started = Arc::new(Notify::new());
    let detached_finished = Arc::new(Notify::new());
    let release_stuck = Arc::new(AtomicBool::new(false));
    let (limit_error_tx, mut limit_error_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let blocking_started = Arc::clone(&blocking_started);
            let stuck_started = Arc::clone(&stuck_started);
            let detached_finished = Arc::clone(&detached_finished);
            let release_stuck = Arc::clone(&release_stuck);
            let limit_error_tx = limit_error_tx.clone();
            move |mut ctx: ActorContext| {
                let blocking_started = Arc::clone(&blocking_started);
                let stuck_started = Arc::clone(&stuck_started);
                let detached_finished = Arc::clone(&detached_finished);
                let release_stuck = Arc::clone(&release_stuck);
                let limit_error_tx = limit_error_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        match envelope.as_slice() {
                            b"saturate" => {
                                let handle = ctx
                                    .spawn_blocking(BlockingOptions::named("first"), {
                                        let blocking_started = Arc::clone(&blocking_started);
                                        move |job| {
                                            blocking_started.notify_one();
                                            loop {
                                                job.checkpoint()?;
                                                thread::sleep(Duration::from_millis(10));
                                            }
                                        }
                                    })
                                    .expect("first blocking task spawned");

                                blocking_started.notified().await;
                                let error = ctx
                                    .spawn_blocking(BlockingOptions::named("second"), |_job| Ok(()))
                                    .expect_err("second blocking task should be rejected");
                                limit_error_tx.send(error).expect("receiver alive");

                                handle.cancel();
                                match handle.wait().await {
                                    Err(BlockingTaskError::Cancelled) => {}
                                    Ok(()) => panic!("blocking task unexpectedly completed"),
                                    Err(other) => panic!("unexpected blocking error: {other}"),
                                }
                            }
                            b"hang-on-shutdown" => {
                                ctx.spawn_blocking(BlockingOptions::named("stuck"), {
                                    let stuck_started = Arc::clone(&stuck_started);
                                    let detached_finished = Arc::clone(&detached_finished);
                                    let release_stuck = Arc::clone(&release_stuck);
                                    move |_job| {
                                        stuck_started.notify_one();
                                        while !release_stuck.load(Ordering::Acquire) {
                                            thread::sleep(Duration::from_millis(10));
                                        }
                                        detached_finished.notify_one();
                                        Ok(())
                                    }
                                })?;
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                }
            }
        }))
        .ingress("requests", "worker")
        .max_blocking_tasks_per_actor(1)
        .blocking_shutdown_timeout(Duration::from_millis(100))
        .build()?;

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    ingress.wait_for_binding().await;
    ingress.send("saturate").await?;

    match timeout(Duration::from_secs(1), limit_error_rx.recv())
        .await?
        .expect("blocking limit result received")
    {
        SpawnBlockingError::AtCapacity {
            actor_id,
            max_blocking_tasks,
        } => {
            println!(
                "actor `{actor_id}` rejected extra blocking work at limit {max_blocking_tasks}"
            );
        }
        other => panic!("unexpected blocking admission error: {other}"),
    }

    ingress.send("hang-on-shutdown").await?;
    stuck_started.notified().await;

    let started_at = Instant::now();
    stop.cancel();
    timeout(Duration::from_secs(1), task)
        .await
        .expect("graph stopped in time")??;
    println!(
        "graph shutdown returned after {:?} even though the blocking task ignored cancellation",
        started_at.elapsed()
    );

    release_stuck.store(true, Ordering::Release);
    timeout(Duration::from_secs(1), detached_finished.notified())
        .await
        .expect("detached blocking task finished after release");
    println!("detached blocking task eventually observed the release signal and exited");

    Ok(())
}
