use std::future::pending;

use tokio::sync::mpsc;
use tokio_actor_typed::{
    Actor, ActorContext, ActorRef, ActorResult, BuildError, GraphBuilder, GraphError, LookupError,
    Reply, SendError,
};
use tokio_util::sync::CancellationToken;

struct Request(&'static str);

#[derive(Clone, PartialEq, Eq, Debug)]
struct Job {
    payload: &'static str,
}

/// Two actors with different message types, wired through a typed ref minted
/// before the target actor is registered. External sends go through the
/// builder-minted ref — no ingress indirection needed.
#[tokio::test]
async fn typed_pipeline_end_to_end() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let worker = builder.declare::<Job>("worker");
    let mut frontend = builder.actor_fn("frontend", move |mut ctx: ActorContext<Request>| {
        let worker = worker.clone();
        async move {
            while let Some(Request(payload)) = ctx.recv().await {
                worker.send(Job { payload }).await?;
            }
            Ok(())
        }
    });
    builder.actor_fn("worker", move |mut ctx: ActorContext<Job>| {
        let seen_tx = seen_tx.clone();
        async move {
            while let Some(job) = ctx.recv().await {
                seen_tx.send(job).expect("receiver alive");
            }
            Ok(())
        }
    });
    let graph = builder.build().expect("valid graph");

    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    frontend.wait_for_binding().await;
    frontend.send(Request("hello")).await.expect("send");

    assert_eq!(seen_rx.recv().await, Some(Job { payload: "hello" }));

    stop.cancel();
    run.await.expect("joined").expect("clean stop");
}

/// Builder-minted refs survive a full stop and rerun of the same graph.
#[tokio::test]
async fn refs_survive_graph_rerun() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let mut echo = builder.actor_fn("echo", move |mut ctx: ActorContext<u32>| {
        let seen_tx = seen_tx.clone();
        async move {
            while let Some(n) = ctx.recv().await {
                seen_tx.send(n).expect("receiver alive");
            }
            Ok(())
        }
    });
    let graph = builder.build().expect("valid graph");

    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });
    echo.wait_for_binding().await;
    echo.send(1).await.expect("send during first run");
    assert_eq!(seen_rx.recv().await, Some(1));
    stop.cancel();
    run.await.expect("joined").expect("clean stop");

    // Between runs the ref is unbound.
    assert!(matches!(
        echo.try_send(2),
        Err(SendError::ActorNotRunning { .. })
    ));

    // The same ref follows the new mailbox on the next run.
    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });
    echo.send_when_ready(3).await.expect("send across restart");
    assert_eq!(seen_rx.recv().await, Some(3));
    stop.cancel();
    run.await.expect("joined").expect("clean stop");
}

enum CounterMsg {
    Add(u64),
    Total(Reply<u64>),
}

#[tokio::test]
async fn call_reply_roundtrip() {
    let mut builder = GraphBuilder::new();
    let mut counter = builder.actor_fn("counter", |mut ctx: ActorContext<CounterMsg>| async move {
        let mut total = 0;
        while let Some(message) = ctx.recv().await {
            match message {
                CounterMsg::Add(n) => total += n,
                CounterMsg::Total(reply) => reply.send(total),
            }
        }
        Ok(())
    });
    let graph = builder.build().expect("valid graph");

    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    counter.wait_for_binding().await;
    counter.send(CounterMsg::Add(1)).await.expect("send");
    counter.send(CounterMsg::Add(2)).await.expect("send");
    let total = counter.call(CounterMsg::Total).await.expect("call");
    assert_eq!(total, 3);

    stop.cancel();
    run.await.expect("joined").expect("clean stop");
}

struct Ball {
    bounces_left: u32,
}

#[derive(Clone)]
struct Paddle {
    other: ActorRef<Ball>,
    done: mpsc::UnboundedSender<()>,
}

impl Actor for Paddle {
    type Msg = Ball;

    async fn run(&self, mut ctx: ActorContext<Ball>) -> ActorResult {
        while let Some(ball) = ctx.recv().await {
            if ball.bounces_left == 0 {
                self.done.send(()).expect("receiver alive");
            } else {
                self.other
                    .send(Ball {
                        bounces_left: ball.bounces_left - 1,
                    })
                    .await?;
            }
        }
        Ok(())
    }
}

/// Mutually referencing actors, wired via `declare` — no ordering problem.
#[tokio::test]
async fn cyclic_wiring_via_declare() {
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let pong = builder.declare::<Ball>("pong");
    let mut ping = builder.actor(
        "ping",
        Paddle {
            other: pong,
            done: done_tx.clone(),
        },
    );
    let ping_for_pong = ping.clone();
    builder.actor(
        "pong",
        Paddle {
            other: ping_for_pong,
            done: done_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let graph = graph.clone();
        let stop = stop.clone();
        async move { graph.run_until(stop.cancelled()).await }
    });

    ping.wait_for_binding().await;
    ping.send(Ball { bounces_left: 5 }).await.expect("serve");
    done_rx.recv().await.expect("rally finished");

    stop.cancel();
    run.await.expect("joined").expect("clean stop");
}

#[test]
fn build_rejects_message_type_mismatch() {
    let mut builder = GraphBuilder::new();
    let _declared = builder.declare::<u32>("worker");
    let _registered = builder.actor_fn("worker", |mut ctx: ActorContext<String>| async move {
        ctx.recv().await;
        Ok(())
    });
    assert!(matches!(
        builder.build(),
        Err(BuildError::MessageTypeMismatch { actor_id, registered, requested })
            if actor_id == "worker" && registered.contains("u32") && requested.contains("String")
    ));
}

#[test]
fn build_rejects_declared_but_missing_actor() {
    let mut builder = GraphBuilder::new();
    let _declared = builder.declare::<u32>("ghost");
    assert!(matches!(
        builder.build(),
        Err(BuildError::MissingActor { actor_id }) if actor_id == "ghost"
    ));
}

#[test]
fn build_rejects_duplicate_actor() {
    async fn noop(mut ctx: ActorContext<u32>) -> ActorResult {
        ctx.recv().await;
        Ok(())
    }

    let mut builder = GraphBuilder::new();
    builder.actor_fn("worker", noop);
    builder.actor_fn("worker", noop);
    assert!(matches!(
        builder.build(),
        Err(BuildError::DuplicateActor { actor_id }) if actor_id == "worker"
    ));
}

#[test]
fn build_rejects_empty_graph() {
    assert!(matches!(
        GraphBuilder::new().build(),
        Err(BuildError::EmptyGraph)
    ));
}

#[tokio::test]
async fn runtime_lookup_checks_message_type() {
    let mut builder = GraphBuilder::new();
    builder.actor_fn("echo", |mut ctx: ActorContext<u32>| async move {
        while ctx.recv().await.is_some() {}
        Ok(())
    });
    let graph = builder.build().expect("valid graph");

    assert!(graph.actor_ref::<u32>("echo").is_ok());
    assert!(matches!(
        graph.actor_ref::<String>("echo"),
        Err(LookupError::MessageTypeMismatch { .. })
    ));
    assert!(matches!(
        graph.actor_ref::<u32>("nope"),
        Err(LookupError::UnknownActor { .. })
    ));
}

#[tokio::test]
async fn actor_error_fails_the_run() {
    let mut builder = GraphBuilder::new();
    builder.actor_fn("healthy", |mut ctx: ActorContext<u32>| async move {
        while ctx.recv().await.is_some() {}
        Ok(())
    });
    builder.actor_fn("bad", |_ctx: ActorContext<u32>| async move {
        Err("boom".into())
    });
    let graph = builder.build().expect("valid graph");

    let result = graph.run_until(pending::<()>()).await;
    assert!(matches!(
        result,
        Err(GraphError::ActorFailed { actor_id, .. }) if actor_id == "bad"
    ));
}

#[tokio::test]
async fn early_clean_exit_fails_the_run() {
    let mut builder = GraphBuilder::new();
    builder.actor_fn("quitter", |_ctx: ActorContext<u32>| async move { Ok(()) });
    let graph = builder.build().expect("valid graph");

    let result = graph.run_until(pending::<()>()).await;
    assert!(matches!(
        result,
        Err(GraphError::ActorExitedEarly { actor_id }) if actor_id == "quitter"
    ));
}

#[tokio::test]
async fn dropped_graph_releases_waiting_refs() {
    let mut builder = GraphBuilder::new();
    let mut echo = builder.actor_fn("echo", |mut ctx: ActorContext<u32>| async move {
        while ctx.recv().await.is_some() {}
        Ok(())
    });
    let graph = builder.build().expect("valid graph");

    drop(graph);
    assert!(!echo.wait_for_binding().await);
}
