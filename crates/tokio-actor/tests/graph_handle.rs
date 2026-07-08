use std::{io, marker::PhantomData, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{Actor, ActorContext, ActorResult, GraphBuilder, GraphError};

struct Drain<M>(PhantomData<fn(M)>);

impl<M> Drain<M> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<M> Clone for Drain<M> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<M: Send + 'static> Actor for Drain<M> {
    type Msg = M;

    async fn run(&self, mut ctx: ActorContext<M>) -> ActorResult {
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[derive(Clone)]
struct Recorder {
    seen: mpsc::UnboundedSender<u32>,
}

impl Actor for Recorder {
    type Msg = u32;

    async fn run(&self, mut ctx: ActorContext<u32>) -> ActorResult {
        while let Some(value) = ctx.recv().await {
            self.seen.send(value).expect("receiver alive");
        }
        Ok(())
    }
}

async fn recv<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("message arrived in time")
        .expect("message observed")
}

#[tokio::test]
async fn spawn_binds_refs_before_returning() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let recorder = builder.actor("recorder", Recorder { seen: seen_tx });
    let graph = builder.build().expect("valid graph");

    let handle = graph.spawn().expect("graph spawned");
    recorder.send(7).await.expect("send without binding wait");

    assert_eq!(recv(&mut seen_rx).await, 7);

    handle
        .shutdown_and_wait()
        .await
        .expect("graph stopped cleanly");
}

#[tokio::test]
async fn shutdown_and_wait_returns_ok_for_healthy_graph() {
    let mut builder = GraphBuilder::new();
    builder.actor("worker", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let handle = graph.spawn().expect("graph spawned");

    handle
        .shutdown_and_wait()
        .await
        .expect("graph stopped cleanly");
}

#[tokio::test]
async fn graph_can_be_respawned_after_shutdown() {
    let mut builder = GraphBuilder::new();
    builder.actor("worker", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let first = graph.spawn().expect("first graph run spawned");
    assert!(matches!(graph.spawn(), Err(GraphError::AlreadyRunning)));

    first
        .shutdown_and_wait()
        .await
        .expect("first run stopped cleanly");

    let second = graph.spawn().expect("second graph run spawned");
    second
        .shutdown_and_wait()
        .await
        .expect("second run stopped cleanly");
}

#[derive(Clone)]
struct Fail;

impl Actor for Fail {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        Err(io::Error::other("boom").into())
    }
}

#[tokio::test]
async fn wait_returns_actor_failed_when_actor_errors() {
    let mut builder = GraphBuilder::new();
    builder.actor("bad", Fail);
    let graph = builder.build().expect("valid graph");

    let handle = graph.spawn().expect("graph spawned");
    let result = handle.wait().await;

    assert!(matches!(
        result,
        Err(GraphError::ActorFailed { actor_id, .. }) if actor_id == "bad"
    ));
}

#[derive(Clone)]
struct Quit;

impl Actor for Quit {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

#[tokio::test]
async fn wait_returns_actor_stopped_when_actor_exits_before_shutdown() {
    let mut builder = GraphBuilder::new();
    builder.actor("quitter", Quit);
    let graph = builder.build().expect("valid graph");

    let handle = graph.spawn().expect("graph spawned");
    let result = handle.wait().await;

    assert!(matches!(
        result,
        Err(GraphError::ActorStopped { actor_id }) if actor_id == "quitter"
    ));
}

#[tokio::test]
async fn shutdown_is_idempotent() {
    let mut builder = GraphBuilder::new();
    builder.actor("worker", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let handle = graph.spawn().expect("graph spawned");
    handle.shutdown();
    handle.shutdown();

    handle.wait().await.expect("graph stopped cleanly");
}
