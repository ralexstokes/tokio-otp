use std::{
    future::pending,
    io,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_actor::{
    Actor, ActorContext, ActorResult, ActorSpec, BlockingOptions, BlockingTaskFailure, BuildError,
    Envelope, Graph, GraphBuilder, GraphError, IngressError,
};
use tokio_util::sync::CancellationToken;

fn start_graph(graph: &Graph) -> (CancellationToken, JoinHandle<Result<(), GraphError>>) {
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });
    (stop, task)
}

async fn stop_graph(stop: CancellationToken, task: JoinHandle<Result<(), GraphError>>) {
    stop.cancel();
    timeout(Duration::from_secs(1), task)
        .await
        .expect("graph stopped in time")
        .expect("graph task joined")
        .expect("graph stopped cleanly");
}

async fn recv_envelope(
    observed_rx: &mut mpsc::UnboundedReceiver<Envelope>,
    message: &str,
) -> Envelope {
    timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect(message)
        .expect("message observed")
}

async fn forward_messages(mut ctx: ActorContext, target: &str) -> ActorResult {
    while let Some(envelope) = ctx.recv().await {
        ctx.send(target, envelope).await?;
    }
    Ok(())
}

async fn observe_messages(
    mut ctx: ActorContext,
    observed_tx: mpsc::UnboundedSender<Envelope>,
) -> ActorResult {
    while let Some(envelope) = ctx.recv().await {
        observed_tx.send(envelope).expect("receiver alive");
    }
    Ok(())
}

async fn drain_mailbox(ctx: &mut ActorContext) {
    while ctx.recv().await.is_some() {}
}

fn oneshot_slot<T>(tx: oneshot::Sender<T>) -> Arc<Mutex<Option<oneshot::Sender<T>>>> {
    Arc::new(Mutex::new(Some(tx)))
}

fn send_once<T>(slot: &Arc<Mutex<Option<oneshot::Sender<T>>>>, value: T) {
    if let Some(tx) = slot.lock().expect("mutex not poisoned").take() {
        let _ = tx.send(value);
    }
}

#[tokio::test]
async fn delivers_messages_across_linked_actors() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("frontend", |ctx: ActorContext| {
            forward_messages(ctx, "worker")
        }))
        .actor(ActorSpec::from_actor("worker", {
            let observed_tx = observed_tx.clone();
            move |ctx: ActorContext| observe_messages(ctx, observed_tx.clone())
        }))
        .link("frontend", "worker")
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph");

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let (stop, task) = start_graph(&graph);

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"hello"))
        .await
        .expect("send succeeded");

    let envelope = recv_envelope(&mut observed_rx, "message arrived in time").await;
    assert_eq!(envelope.as_slice(), b"hello");

    stop_graph(stop, task).await;
}

#[derive(Clone)]
struct ForwardingActor;

impl Actor for ForwardingActor {
    fn run(&self, ctx: ActorContext) -> impl std::future::Future<Output = ActorResult> + Send {
        forward_messages(ctx, "worker")
    }
}

#[derive(Clone)]
struct ObservingActor {
    observed_tx: mpsc::UnboundedSender<Envelope>,
}

impl Actor for ObservingActor {
    fn run(&self, ctx: ActorContext) -> impl std::future::Future<Output = ActorResult> + Send {
        observe_messages(ctx, self.observed_tx.clone())
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

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let (stop, task) = start_graph(&graph);

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"hello"))
        .await
        .expect("send succeeded");

    let envelope = recv_envelope(&mut observed_rx, "message arrived in time").await;
    assert_eq!(envelope.as_slice(), b"hello");

    stop_graph(stop, task).await;
}

#[tokio::test]
async fn ingress_handle_rebinds_across_reruns() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("frontend", {
            let observed_tx = observed_tx.clone();
            move |ctx: ActorContext| observe_messages(ctx, observed_tx.clone())
        }))
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph");

    let mut ingress = graph.ingress("requests").expect("ingress exists");

    let (first_stop, first_run) = start_graph(&graph);
    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"first"))
        .await
        .expect("send succeeded");
    let first = recv_envelope(&mut observed_rx, "first message arrived").await;
    assert_eq!(first.as_slice(), b"first");

    stop_graph(first_stop, first_run).await;

    let not_running = ingress.send(Envelope::from_static(b"stopped")).await;
    assert_eq!(
        not_running,
        Err(IngressError::NotRunning {
            ingress: "requests".to_owned(),
            actor_id: "frontend".to_owned(),
        })
    );

    let (second_stop, second_run) = start_graph(&graph);
    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"second"))
        .await
        .expect("send succeeded");
    let second = recv_envelope(&mut observed_rx, "second message arrived").await;
    assert_eq!(second.as_slice(), b"second");

    stop_graph(second_stop, second_run).await;
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
    let started_tx = oneshot_slot(started_tx);
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let started_tx = Arc::clone(&started_tx);
            move |mut ctx: ActorContext| {
                let started_tx = Arc::clone(&started_tx);
                async move {
                    send_once(&started_tx, ());
                    drain_mailbox(&mut ctx).await;
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph");

    let (stop, task) = start_graph(&graph);

    started_rx.await.expect("actor started");
    stop_graph(stop, task).await;
}

#[tokio::test]
async fn graph_can_only_run_once_at_a_time() {
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = oneshot_slot(entered_tx);
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let entered_tx = Arc::clone(&entered_tx);
            move |mut ctx: ActorContext| {
                let entered_tx = Arc::clone(&entered_tx);
                async move {
                    send_once(&entered_tx, ());
                    drain_mailbox(&mut ctx).await;
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph");

    let (stop, first_run) = start_graph(&graph);
    entered_rx.await.expect("first actor started");

    let second_run = graph.run_until(async {}).await;
    assert!(matches!(second_run, Err(GraphError::AlreadyRunning)));

    stop_graph(stop, first_run).await;
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

                drain_mailbox(&mut ctx).await;
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
    let started_tx = oneshot_slot(started_tx);
    let cleaned_tx = oneshot_slot(cleaned_tx);

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let started_tx = Arc::clone(&started_tx);
            let cleaned_tx = Arc::clone(&cleaned_tx);
            move |mut ctx: ActorContext| {
                let started_tx = Arc::clone(&started_tx);
                let cleaned_tx = Arc::clone(&cleaned_tx);
                async move {
                    ctx.spawn_blocking(BlockingOptions::named("cleanup"), move |job| {
                        send_once(&started_tx, ());

                        while job.checkpoint().is_ok() {
                            thread::sleep(Duration::from_millis(10));
                        }

                        send_once(&cleaned_tx, ());
                        Ok(())
                    })
                    .expect("blocking task spawned");

                    drain_mailbox(&mut ctx).await;
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph");

    let (stop, task) = start_graph(&graph);

    started_rx.await.expect("blocking task started");
    stop_graph(stop, task).await;
    timeout(Duration::from_secs(1), cleaned_rx)
        .await
        .expect("cleanup finished before graph returned")
        .expect("cleanup signal received");
}

#[tokio::test]
async fn awaited_blocking_task_failures_can_be_handled_locally() {
    let (handled_tx, handled_rx) = oneshot::channel();
    let handled_tx = oneshot_slot(handled_tx);

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let handled_tx = Arc::clone(&handled_tx);
            move |mut ctx: ActorContext| {
                let handled_tx = Arc::clone(&handled_tx);
                async move {
                    let handle = ctx
                        .spawn_blocking(BlockingOptions::named("boom"), |_job| {
                            Err(io::Error::other("boom").into())
                        })
                        .expect("blocking task spawned");
                    handle.wait().await.expect_err("blocking task should fail");

                    send_once(&handled_tx, ());

                    drain_mailbox(&mut ctx).await;
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph");

    let (stop, task) = start_graph(&graph);

    handled_rx.await.expect("actor handled blocking failure");
    stop_graph(stop, task).await;
}

#[tokio::test]
async fn blocking_task_failure_fails_uncooperative_actor_promptly() {
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "worker",
            |ctx: ActorContext| async move {
                ctx.spawn_blocking(BlockingOptions::named("boom"), |_job| {
                    Err(io::Error::other("boom").into())
                })
                .expect("blocking task spawned");

                pending::<ActorResult>().await
            },
        ))
        .build()
        .expect("valid graph");

    let result = timeout(Duration::from_secs(1), graph.run_until(pending::<()>()))
        .await
        .expect("graph returned in time");
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
async fn actor_exit_is_not_masked_by_shutdown_after_it_finishes() {
    let (done_tx, done_rx) = oneshot::channel();
    let done_tx = oneshot_slot(done_tx);

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let done_tx = Arc::clone(&done_tx);
            move |_ctx: ActorContext| {
                let done_tx = Arc::clone(&done_tx);
                async move {
                    send_once(&done_tx, ());
                    Ok(())
                }
            }
        }))
        .build()
        .expect("valid graph");

    let result = graph
        .run_until(async move {
            done_rx.await.expect("actor finished");
            tokio::task::yield_now().await;
        })
        .await;

    assert!(matches!(
        result,
        Err(GraphError::ActorStopped { actor_id }) if actor_id == "worker"
    ));
}

#[tokio::test]
async fn run_blocking_does_not_deadlock_on_self_mailbox_backpressure() {
    let (finished_tx, finished_rx) = oneshot::channel();
    let finished_tx = oneshot_slot(finished_tx);

    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", {
            let finished_tx = Arc::clone(&finished_tx);
            move |mut ctx: ActorContext| {
                let finished_tx = Arc::clone(&finished_tx);
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        if envelope.as_slice() == b"start" {
                            ctx.myself()
                                .try_send(Envelope::from_static(b"queued"))
                                .expect("mailbox slot available");

                            ctx.run_blocking(BlockingOptions::named("self-send"), |job| {
                                job.myself()
                                    .blocking_send(Envelope::from_static(b"result"))?;
                                Ok(())
                            })
                            .await
                            .expect_err("self-send should surface mailbox pressure");

                            send_once(&finished_tx, ());
                        }
                    }
                    Ok(())
                }
            }
        }))
        .ingress("requests", "worker")
        .mailbox_capacity(1)
        .build()
        .expect("valid graph");

    let mut ingress = graph.ingress("requests").expect("ingress exists");
    let (stop, task) = start_graph(&graph);

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"start"))
        .await
        .expect("send succeeded");

    timeout(Duration::from_secs(1), finished_rx)
        .await
        .expect("actor did not deadlock")
        .expect("actor reported completion");

    stop_graph(stop, task).await;
}
