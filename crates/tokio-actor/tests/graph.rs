use std::{
    io,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    time::{sleep, timeout},
};
use tokio_actor::{
    Actor, ActorContext, ActorResult, ActorSpec, BlockingOptions, BlockingTaskFailure, BuildError,
    Envelope, GraphBuilder, GraphError, IngressError,
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn delivers_messages_across_linked_actors() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "frontend",
            |mut ctx: ActorContext| async move {
                while let Some(envelope) = ctx.recv().await {
                    ctx.send("worker", envelope).await?;
                }
                Ok(())
            },
        ))
        .actor(ActorSpec::from_actor("worker", {
            let observed_tx = observed_tx.clone();
            move |mut ctx: ActorContext| {
                let observed_tx = observed_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        observed_tx.send(envelope).expect("receiver alive");
                    }
                    Ok(())
                }
            }
        }))
        .link("frontend", "worker")
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph");

    let ingress = graph.ingress("requests").expect("ingress exists");
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    send_when_ready(&ingress, Envelope::from_static(b"hello")).await;

    let envelope = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("message arrived in time")
        .expect("message observed");
    assert_eq!(envelope.as_slice(), b"hello");

    stop.cancel();
    task.await
        .expect("graph task joined")
        .expect("graph stopped cleanly");
}

#[derive(Clone)]
struct ForwardingActor;

impl Actor for ForwardingActor {
    async fn run(&self, mut ctx: ActorContext) -> ActorResult {
        while let Some(envelope) = ctx.recv().await {
            ctx.send("worker", envelope).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ObservingActor {
    observed_tx: mpsc::UnboundedSender<Envelope>,
}

impl Actor for ObservingActor {
    async fn run(&self, mut ctx: ActorContext) -> ActorResult {
        while let Some(envelope) = ctx.recv().await {
            self.observed_tx.send(envelope).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn delivers_messages_with_trait_actors() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("frontend", ForwardingActor))
        .actor(ActorSpec::from_actor(
            "worker",
            ObservingActor {
                observed_tx: observed_tx.clone(),
            },
        ))
        .link("frontend", "worker")
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph");

    let ingress = graph.ingress("requests").expect("ingress exists");
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    send_when_ready(&ingress, Envelope::from_static(b"hello")).await;

    let envelope = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("message arrived in time")
        .expect("message observed");
    assert_eq!(envelope.as_slice(), b"hello");

    stop.cancel();
    task.await
        .expect("graph task joined")
        .expect("graph stopped cleanly");
}

#[tokio::test]
async fn ingress_handle_rebinds_across_reruns() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("frontend", {
            let observed_tx = observed_tx.clone();
            move |mut ctx: ActorContext| {
                let observed_tx = observed_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        observed_tx.send(envelope).expect("receiver alive");
                    }
                    Ok(())
                }
            }
        }))
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph");

    let ingress = graph.ingress("requests").expect("ingress exists");

    let first_stop = CancellationToken::new();
    let first_run = tokio::spawn({
        let graph = graph.clone();
        let first_stop = first_stop.clone();
        async move { graph.run_until(first_stop.cancelled()).await }
    });
    send_when_ready(&ingress, Envelope::from_static(b"first")).await;
    let first = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("first message arrived")
        .expect("message observed");
    assert_eq!(first.as_slice(), b"first");

    first_stop.cancel();
    first_run
        .await
        .expect("first run joined")
        .expect("first run stopped cleanly");

    let not_running = ingress.send(Envelope::from_static(b"stopped")).await;
    assert_eq!(
        not_running,
        Err(IngressError::NotRunning {
            ingress: "requests".to_owned(),
            actor_id: "frontend".to_owned(),
        })
    );

    let second_stop = CancellationToken::new();
    let second_run = tokio::spawn({
        let graph = graph.clone();
        let second_stop = second_stop.clone();
        async move { graph.run_until(second_stop.cancelled()).await }
    });
    send_when_ready(&ingress, Envelope::from_static(b"second")).await;
    let second = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("second message arrived")
        .expect("message observed");
    assert_eq!(second.as_slice(), b"second");

    second_stop.cancel();
    second_run
        .await
        .expect("second run joined")
        .expect("second run stopped cleanly");
}

#[tokio::test]
async fn rejects_invalid_graph_definitions() {
    let duplicate = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |_ctx: ActorContext| async { Ok(()) },
        ))
        .actor(ActorSpec::from_actor(
            "worker",
            |_ctx: ActorContext| async { Ok(()) },
        ))
        .build();
    assert!(matches!(
        duplicate,
        Err(BuildError::DuplicateActorId(actor_id)) if actor_id == "worker"
    ));

    let unknown_link = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |_ctx: ActorContext| async { Ok(()) },
        ))
        .link("worker", "missing")
        .build();
    assert!(matches!(
        unknown_link,
        Err(BuildError::UnknownLinkTarget { from, actor })
            if from == "worker" && actor == "missing"
    ));
}

#[tokio::test]
async fn actor_error_fails_the_graph() {
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |_ctx: ActorContext| async move {
                Err::<(), _>(Box::<dyn std::error::Error + Send + Sync>::from(
                    io::Error::other("boom"),
                ))
            },
        ))
        .build()
        .expect("valid graph");

    let result = graph.run_until(async {}).await;
    match result {
        Err(GraphError::ActorFailed { actor_id, .. }) => assert_eq!(actor_id, "worker"),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[tokio::test]
async fn graph_shutdown_is_cooperative() {
    let (started_tx, started_rx) = oneshot::channel();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let started_tx = Arc::clone(&started_tx);
            move |mut ctx: ActorContext| {
                let started_tx = Arc::clone(&started_tx);
                async move {
                    if let Some(tx) = started_tx.lock().expect("mutex not poisoned").take() {
                        let _ = tx.send(());
                    }

                    while ctx.recv().await.is_some() {}
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph");

    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    started_rx.await.expect("actor started");
    stop.cancel();
    timeout(Duration::from_secs(1), task)
        .await
        .expect("graph stopped in time")
        .expect("graph task joined")
        .expect("graph stopped cleanly");
}

#[tokio::test]
async fn graph_can_only_run_once_at_a_time() {
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let entered_tx = Arc::clone(&entered_tx);
            move |mut ctx: ActorContext| {
                let entered_tx = Arc::clone(&entered_tx);
                async move {
                    if let Some(tx) = entered_tx.lock().expect("mutex not poisoned").take() {
                        let _ = tx.send(());
                    }
                    while ctx.recv().await.is_some() {}
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph");

    let stop = CancellationToken::new();
    let first_run = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });
    entered_rx.await.expect("first actor started");

    let second_run = graph.run_until(async {}).await;
    assert!(matches!(second_run, Err(GraphError::AlreadyRunning)));

    stop.cancel();
    first_run
        .await
        .expect("first run joined")
        .expect("first run stopped cleanly");
}

#[tokio::test]
async fn dropped_blocking_task_failures_fail_the_actor() {
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |mut ctx: ActorContext| async move {
                ctx.spawn_blocking(BlockingOptions::named("boom"), |_job| {
                    Err(io::Error::other("boom").into())
                })
                .expect("blocking task spawned");

                while ctx.recv().await.is_some() {}
                Ok(())
            },
        ))
        .build()
        .expect("valid graph");

    let result = graph
        .run_until(tokio::time::sleep(Duration::from_secs(1)))
        .await;
    match result {
        Err(GraphError::ActorFailed { actor_id, source }) => {
            assert_eq!(actor_id, "worker");
            let failure = source
                .downcast_ref::<BlockingTaskFailure>()
                .expect("blocking failure is attached");
            assert_eq!(failure.task_name(), Some("boom"));
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[tokio::test]
async fn graph_waits_for_dropped_blocking_tasks_to_cleanup() {
    let (started_tx, started_rx) = oneshot::channel();
    let (cleaned_tx, cleaned_rx) = oneshot::channel();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let cleaned_tx = Arc::new(Mutex::new(Some(cleaned_tx)));

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let started_tx = Arc::clone(&started_tx);
            let cleaned_tx = Arc::clone(&cleaned_tx);
            move |mut ctx: ActorContext| {
                let started_tx = Arc::clone(&started_tx);
                let cleaned_tx = Arc::clone(&cleaned_tx);
                async move {
                    ctx.spawn_blocking(BlockingOptions::named("cleanup"), move |job| {
                        if let Some(tx) = started_tx.lock().expect("mutex not poisoned").take() {
                            let _ = tx.send(());
                        }

                        loop {
                            if job.checkpoint().is_err() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }

                        if let Some(tx) = cleaned_tx.lock().expect("mutex not poisoned").take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .expect("blocking task spawned");

                    while ctx.recv().await.is_some() {}
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph");

    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    started_rx.await.expect("blocking task started");
    stop.cancel();
    task.await
        .expect("graph task joined")
        .expect("graph stopped cleanly");
    timeout(Duration::from_secs(1), cleaned_rx)
        .await
        .expect("cleanup finished before graph returned")
        .expect("cleanup signal received");
}

async fn send_when_ready(ingress: &tokio_actor::IngressHandle, envelope: Envelope) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        match ingress.send(envelope.clone()).await {
            Ok(()) => return,
            Err(IngressError::NotRunning { .. }) if tokio::time::Instant::now() < deadline => {
                sleep(Duration::from_millis(10)).await;
            }
            Err(err) => panic!("unexpected ingress error: {err}"),
        }
    }
}
