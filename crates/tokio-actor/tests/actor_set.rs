use std::{
    future::pending,
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_actor::{
    ActorContext, ActorRunError, ActorSet, ActorSpec, BoxError, Envelope, GraphBuilder,
    RunnableActor, SendError,
};
use tokio_util::sync::CancellationToken;

fn start_actor(actor: RunnableActor) -> (CancellationToken, JoinHandle<Result<(), ActorRunError>>) {
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let stop = stop.clone();
        async move { actor.run_until(stop.cancelled()).await }
    });
    (stop, task)
}

async fn stop_actor(
    stop: CancellationToken,
    task: JoinHandle<Result<(), ActorRunError>>,
) -> Result<(), ActorRunError> {
    stop.cancel();
    timeout(Duration::from_secs(1), task)
        .await
        .expect("actor stopped in time")
        .expect("actor task joined")
}

fn single_actor_set(actor_set: &ActorSet, id: &str) -> RunnableActor {
    actor_set.actor(id).expect("actor exists").clone()
}

async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
    timeout(Duration::from_secs(1), async {
        loop {
            if counter.load(Ordering::SeqCst) == expected {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("counter reached expected value in time");
}

#[tokio::test]
async fn actor_ref_reports_not_running_for_stopped_peer() {
    let (errors_tx, mut errors_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("frontend", {
            let errors_tx = errors_tx.clone();
            move |mut ctx: ActorContext| {
                let errors_tx = errors_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        let error = ctx
                            .send("worker", envelope)
                            .await
                            .expect_err("worker is not running");
                        errors_tx.send(error).expect("receiver alive");
                    }
                    Ok(())
                }
            }
        }))
        .actor(ActorSpec::from_actor(
            "worker",
            |_ctx: ActorContext| async move { Ok(()) },
        ))
        .link("frontend", "worker")
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let frontend = single_actor_set(&graph, "frontend");
    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let (stop, task) = start_actor(frontend);

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"hello"))
        .await
        .expect("frontend accepts ingress message");

    let error = timeout(Duration::from_secs(1), errors_rx.recv())
        .await
        .expect("frontend reported the failure in time")
        .expect("frontend reported a failure");
    assert_eq!(
        error,
        SendError::ActorNotRunning {
            actor_id: "worker".to_owned(),
        }
    );

    stop_actor(stop, task)
        .await
        .expect("frontend stopped cleanly");
}

#[tokio::test]
async fn send_when_ready_returns_promptly_on_shutdown() {
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
        .actor(ActorSpec::from_actor(
            "worker",
            |_ctx: ActorContext| async move { Ok(()) },
        ))
        .link("frontend", "worker")
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let frontend = single_actor_set(&graph, "frontend");
    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let (stop, task) = start_actor(frontend);

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"hello"))
        .await
        .expect("frontend accepts ingress message");

    stop_actor(stop, task)
        .await
        .expect("frontend stopped cleanly while waiting for worker restart");
}

#[tokio::test]
async fn runnable_actor_rejects_concurrent_runs() {
    let actor_set = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |mut ctx: ActorContext| async move {
                while ctx.recv().await.is_some() {}
                Ok(())
            },
        ))
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let worker = single_actor_set(&actor_set, "worker");
    let (stop, task) = start_actor(worker.clone());
    sleep(Duration::from_millis(20)).await;

    assert!(matches!(
        worker.run_until(pending::<()>()).await,
        Err(ActorRunError::AlreadyRunning { actor_id }) if actor_id == "worker"
    ));

    stop_actor(stop, task)
        .await
        .expect("worker stopped cleanly");
}

#[tokio::test]
async fn aborting_run_until_aborts_inner_actor_task() {
    struct LiveGuard(Arc<AtomicUsize>);

    impl Drop for LiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let actor_set = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let started_tx = started_tx.clone();
            move |_ctx: ActorContext| {
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                let started_tx = started_tx.clone();
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    let _guard = LiveGuard(active);
                    started_tx.send(()).expect("receiver alive");
                    pending::<()>().await;
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let worker = single_actor_set(&actor_set, "worker");
    let first_worker = worker.clone();
    let first_task = tokio::spawn(async move { first_worker.run_until(pending::<()>()).await });

    timeout(Duration::from_secs(1), started_rx.recv())
        .await
        .expect("first actor started in time")
        .expect("first actor reported start");

    first_task.abort();
    assert!(
        first_task
            .await
            .expect_err("outer run task should be cancelled")
            .is_cancelled()
    );
    wait_for_count(&active, 0).await;

    let second_worker = worker.clone();
    let second_task = tokio::spawn(async move { second_worker.run_until(pending::<()>()).await });
    timeout(Duration::from_secs(1), started_rx.recv())
        .await
        .expect("second actor started in time")
        .expect("second actor reported start");

    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "actor instances must not overlap across aborted run_until restarts"
    );

    second_task.abort();
    assert!(
        second_task
            .await
            .expect_err("outer run task should be cancelled")
            .is_cancelled()
    );
    wait_for_count(&active, 0).await;
}

#[tokio::test]
async fn actor_set_preserves_wiring_across_individual_restarts() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let runs = Arc::new(AtomicUsize::new(0));

    let actor_set = GraphBuilder::new()
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
            let observed_tx = observed_tx.clone();
            let runs = Arc::clone(&runs);
            move |mut ctx: ActorContext| {
                let observed_tx = observed_tx.clone();
                let run = runs.fetch_add(1, Ordering::SeqCst);
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        observed_tx.send(envelope).expect("receiver alive");
                        if run == 0 {
                            return Err::<(), BoxError>(Box::new(io::Error::other("boom")));
                        }
                    }
                    Ok(())
                }
            }
        }))
        .link("frontend", "worker")
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let frontend = single_actor_set(&actor_set, "frontend");
    let worker = single_actor_set(&actor_set, "worker");
    let mut ingress = actor_set.ingress("requests").expect("ingress exists");
    let (frontend_stop, frontend_task) = start_actor(frontend);
    let (_first_worker_stop, first_worker_task) = start_actor(worker.clone());

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"first"))
        .await
        .expect("frontend accepted first message");
    let first = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("first worker run observed a message")
        .expect("worker reported a message");
    assert_eq!(first.as_slice(), b"first");

    let first_exit = timeout(Duration::from_secs(1), first_worker_task)
        .await
        .expect("first worker run exited in time")
        .expect("first worker task joined");
    assert!(matches!(
        first_exit,
        Err(ActorRunError::Failed { ref actor_id, .. }) if actor_id == "worker"
    ));

    ingress
        .send(Envelope::from_static(b"second"))
        .await
        .expect("frontend accepted second message");
    assert!(
        timeout(Duration::from_millis(100), observed_rx.recv())
            .await
            .is_err(),
        "frontend should wait for the worker to restart"
    );

    let (second_worker_stop, second_worker_task) = start_actor(worker);
    let second = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("second worker run observed a message")
        .expect("worker reported a message");
    assert_eq!(second.as_slice(), b"second");

    stop_actor(frontend_stop, frontend_task)
        .await
        .expect("frontend stopped cleanly");
    stop_actor(second_worker_stop, second_worker_task)
        .await
        .expect("restarted worker stopped cleanly");
}
