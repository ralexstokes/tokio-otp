use std::time::Duration;

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{Actor, ActorContext, ActorSpec, Envelope, GraphBuilder};
use tokio_otp::{Runtime, SupervisedActors};
use tokio_supervisor::{Strategy, SupervisorBuilder, SupervisorExit, SupervisorStateView};

fn build_runtime<A>(actor: A) -> Runtime
where
    A: Actor,
{
    let graph = GraphBuilder::new()
        .actor(ActorSpec::from_actor("worker", actor))
        .ingress("requests", "worker")
        .build()
        .expect("valid graph");

    SupervisedActors::new(graph)
        .expect("graph decomposes")
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("runtime builds")
}

#[tokio::test]
async fn runtime_spawn_combines_ingress_and_supervisor_control() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let runtime = build_runtime(move |mut ctx: ActorContext| {
        let observed_tx = observed_tx.clone();
        Box::pin(async move {
            while let Some(envelope) = ctx.recv().await {
                observed_tx.send(envelope).expect("receiver alive");
            }
            Ok(())
        })
    });

    let handle = runtime.spawn();
    let supervisor_handle = handle.supervisor_handle();
    let mut ingress = handle.ingress("requests").expect("ingress exists");

    assert_eq!(
        supervisor_handle.snapshot().state,
        SupervisorStateView::Running
    );
    assert_eq!(handle.snapshot().children.len(), 1);

    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"hello"))
        .await
        .expect("message sent");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker observed the message")
        .expect("worker is still running");
    assert_eq!(observed.as_slice(), b"hello");

    assert_eq!(
        handle
            .shutdown_and_wait()
            .await
            .expect("supervisor shut down cleanly"),
        SupervisorExit::Shutdown
    );
}

#[tokio::test]
async fn runtime_run_accepts_ingress_cloned_before_startup() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let runtime = build_runtime(move |mut ctx: ActorContext| {
        let observed_tx = observed_tx.clone();
        Box::pin(async move {
            let envelope = ctx.recv().await.expect("message received before shutdown");
            observed_tx.send(envelope).expect("receiver alive");
            Ok(())
        })
    });

    let mut ingress = runtime.ingress("requests").expect("ingress exists");
    let sender = tokio::spawn(async move {
        ingress.wait_for_binding().await;
        ingress
            .send(Envelope::from_static(b"run-path"))
            .await
            .expect("message sent through cloned ingress");
    });

    assert_eq!(
        runtime.run().await.expect("runtime exits cleanly"),
        SupervisorExit::Completed
    );
    sender.await.expect("sender task joined");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker observed the message")
        .expect("worker is still running");
    assert_eq!(observed.as_slice(), b"run-path");
}

#[tokio::test]
async fn runtime_handle_returns_none_for_unknown_ingress() {
    let runtime = build_runtime(|ctx: ActorContext| {
        Box::pin(async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        })
    });

    let handle = runtime.spawn();
    assert!(handle.ingress("missing").is_none());

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}

#[tokio::test]
async fn runtime_into_parts_round_trips_raw_pieces() {
    let runtime = build_runtime(|ctx: ActorContext| {
        Box::pin(async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        })
    });

    let (supervisor, ingresses) = runtime.into_parts();
    assert!(ingresses.contains_key("requests"));

    let runtime = Runtime::new(supervisor, ingresses);
    assert!(runtime.ingress("requests").is_some());

    let handle = runtime.spawn();
    let mut ingress = handle.ingress("requests").expect("ingress exists");
    ingress.wait_for_binding().await;

    assert_eq!(
        handle
            .shutdown_and_wait()
            .await
            .expect("supervisor shut down cleanly"),
        SupervisorExit::Shutdown
    );
}
