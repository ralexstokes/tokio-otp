use std::{
    future::pending,
    io,
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_actor::{
    Actor, ActorContext, ActorRef, ActorResult, BlockingOptions, BlockingTaskFailure, BuildError,
    Graph, GraphBuilder, GraphError, LookupError, Reply, SendError,
};
use tokio_util::sync::CancellationToken;

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

async fn recv<T>(rx: &mut mpsc::UnboundedReceiver<T>, message: &str) -> T {
    timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect(message)
        .expect("message observed")
}

struct Request(&'static str);

#[derive(Clone, Debug, Eq, PartialEq)]
struct Job {
    payload: &'static str,
}

#[derive(Clone)]
struct Frontend {
    worker: ActorRef<Job>,
}

impl Actor for Frontend {
    type Msg = Request;

    async fn run(&self, mut ctx: ActorContext<Request>) -> ActorResult {
        while let Some(Request(payload)) = ctx.recv().await {
            self.worker.send(Job { payload }).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Worker {
    seen: mpsc::UnboundedSender<Job>,
}

impl Actor for Worker {
    type Msg = Job;

    async fn run(&self, mut ctx: ActorContext<Job>) -> ActorResult {
        while let Some(job) = ctx.recv().await {
            self.seen.send(job).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn typed_pipeline_end_to_end() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let worker = builder.declare::<Job>("worker");
    let mut frontend = builder.actor("frontend", Frontend { worker });
    builder.actor("worker", Worker { seen: seen_tx });
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    frontend.wait_for_binding().await;
    frontend.send(Request("hello")).await.expect("send");

    assert_eq!(
        recv(&mut seen_rx, "message arrived").await,
        Job { payload: "hello" }
    );

    stop_graph(stop, task).await;
}

#[derive(Clone)]
struct Echo {
    seen: mpsc::UnboundedSender<u32>,
}

impl Actor for Echo {
    type Msg = u32;

    async fn run(&self, mut ctx: ActorContext<u32>) -> ActorResult {
        while let Some(n) = ctx.recv().await {
            self.seen.send(n).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn refs_survive_graph_rerun() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let mut echo = builder.actor("echo", Echo { seen: seen_tx });
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    echo.wait_for_binding().await;
    echo.send(1).await.expect("send during first run");
    assert_eq!(recv(&mut seen_rx, "first message").await, 1);
    stop_graph(stop, task).await;

    assert!(matches!(
        echo.try_send(2),
        Err(SendError::ActorNotRunning { .. })
    ));

    let (stop, task) = start_graph(&graph);
    echo.send_when_ready(3).await.expect("send across rerun");
    assert_eq!(recv(&mut seen_rx, "second message").await, 3);
    stop_graph(stop, task).await;
}

enum CounterMsg {
    Add(u64),
    Total(Reply<u64>),
}

#[derive(Clone)]
struct Counter;

impl Actor for Counter {
    type Msg = CounterMsg;

    async fn run(&self, mut ctx: ActorContext<CounterMsg>) -> ActorResult {
        let mut total = 0;
        while let Some(message) = ctx.recv().await {
            match message {
                CounterMsg::Add(n) => total += n,
                CounterMsg::Total(reply) => reply.send(total),
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn call_reply_roundtrip() {
    let mut builder = GraphBuilder::new();
    let mut counter = builder.actor("counter", Counter);
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    counter.wait_for_binding().await;
    counter.send(CounterMsg::Add(1)).await.expect("send");
    counter.send(CounterMsg::Add(2)).await.expect("send");

    assert_eq!(counter.call(CounterMsg::Total).await.expect("call"), 3);

    stop_graph(stop, task).await;
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
    builder.actor(
        "pong",
        Paddle {
            other: ping.clone(),
            done: done_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    ping.wait_for_binding().await;
    ping.send(Ball { bounces_left: 5 }).await.expect("serve");
    recv(&mut done_rx, "rally finished").await;
    stop_graph(stop, task).await;
}

#[test]
fn graph_preserves_explicit_name() {
    let mut builder = GraphBuilder::new();
    builder.name("orders");
    builder.actor("worker", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    assert_eq!(graph.name(), "orders");
}

#[test]
fn graph_generates_unique_anonymous_names() {
    let mut first = GraphBuilder::new();
    first.actor("worker", Drain::<()>::new());
    let first = first.build().expect("valid graph");

    let mut second = GraphBuilder::new();
    second.actor("worker", Drain::<()>::new());
    let second = second.build().expect("valid graph");

    assert_ne!(first.name(), second.name());
    assert!(first.name().starts_with("graph-"));
    assert!(second.name().starts_with("graph-"));
}

#[test]
fn build_rejects_invalid_graph_definitions() {
    let mut mismatch = GraphBuilder::new();
    mismatch.declare::<u32>("worker");
    mismatch.actor("worker", Drain::<String>::new());
    assert!(matches!(
        mismatch.build(),
        Err(BuildError::MessageTypeMismatch { actor_id, registered, requested })
            if actor_id == "worker" && registered.contains("u32") && requested.contains("String")
    ));

    let mut missing = GraphBuilder::new();
    missing.declare::<u32>("ghost");
    assert!(matches!(
        missing.build(),
        Err(BuildError::MissingActor { actor_id }) if actor_id == "ghost"
    ));

    let mut duplicate = GraphBuilder::new();
    duplicate.actor("worker", Drain::<u32>::new());
    duplicate.actor("worker", Drain::<u32>::new());
    assert!(matches!(
        duplicate.build(),
        Err(BuildError::DuplicateActorId { actor_id }) if actor_id == "worker"
    ));

    let empty = GraphBuilder::new();
    assert!(matches!(empty.build(), Err(BuildError::EmptyGraph)));

    let mut empty_name = GraphBuilder::new();
    empty_name.name("");
    empty_name.actor("worker", Drain::<()>::new());
    assert!(matches!(
        empty_name.build(),
        Err(BuildError::InvalidConfig("graph name must not be empty"))
    ));

    let mut zero_capacity = GraphBuilder::new();
    zero_capacity.mailbox_capacity(0);
    zero_capacity.actor("worker", Drain::<()>::new());
    assert!(matches!(
        zero_capacity.build(),
        Err(BuildError::InvalidConfig(
            "mailbox capacity must be non-zero"
        ))
    ));
}

#[test]
fn runtime_lookup_checks_message_type() {
    let mut builder = GraphBuilder::new();
    builder.actor("echo", Drain::<u32>::new());
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

#[derive(Clone)]
struct Fail;

impl Actor for Fail {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        Err(io::Error::other("boom").into())
    }
}

#[tokio::test]
async fn actor_error_fails_the_graph() {
    let mut builder = GraphBuilder::new();
    builder.actor("healthy", Drain::<()>::new());
    builder.actor("bad", Fail);
    let graph = builder.build().expect("valid graph");

    let result = graph.run_until(pending::<()>()).await;
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
async fn early_clean_exit_fails_the_graph() {
    let mut builder = GraphBuilder::new();
    builder.actor("quitter", Quit);
    let graph = builder.build().expect("valid graph");

    let result = graph.run_until(pending::<()>()).await;
    assert!(matches!(
        result,
        Err(GraphError::ActorStopped { actor_id }) if actor_id == "quitter"
    ));
}

#[tokio::test]
async fn graph_can_only_run_once_at_a_time() {
    let mut builder = GraphBuilder::new();
    let mut worker = builder.actor("worker", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    worker.wait_for_binding().await;
    assert!(matches!(
        graph.run_until(async {}).await,
        Err(GraphError::AlreadyRunning)
    ));
    stop_graph(stop, task).await;
}

#[tokio::test]
async fn graph_shutdown_aborts_uncooperative_actor_after_timeout() {
    struct LiveGuard(Arc<AtomicBool>);

    impl Drop for LiveGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    #[derive(Clone)]
    struct Stubborn {
        started: Arc<Notify>,
        live: Arc<AtomicBool>,
    }

    impl Actor for Stubborn {
        type Msg = ();

        async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
            self.live.store(true, Ordering::Release);
            let _guard = LiveGuard(self.live.clone());
            self.started.notify_one();
            pending::<()>().await;
            Ok(())
        }
    }

    let started = Arc::new(Notify::new());
    let live = Arc::new(AtomicBool::new(false));
    let mut builder = GraphBuilder::new();
    builder.actor_shutdown_timeout(Duration::from_millis(50));
    builder.actor(
        "worker",
        Stubborn {
            started: started.clone(),
            live: live.clone(),
        },
    );
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    started.notified().await;
    stop_graph(stop, task).await;
    assert!(
        !live.load(Ordering::Acquire),
        "uncooperative actor should be aborted before graph shutdown returns"
    );
}

#[derive(Clone)]
struct BlockingFailure;

impl Actor for BlockingFailure {
    type Msg = ();

    async fn run(&self, mut ctx: ActorContext<()>) -> ActorResult {
        ctx.spawn_blocking(BlockingOptions::named("boom"), |_job| {
            Err(io::Error::other("boom").into())
        })
        .expect("blocking task spawned");

        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[tokio::test]
async fn dropped_blocking_task_failures_fail_the_actor() {
    let mut builder = GraphBuilder::new();
    builder.actor("worker", BlockingFailure);
    let graph = builder.build().expect("valid graph");

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

type SenderSlot<T> = Arc<Mutex<Option<oneshot::Sender<T>>>>;

fn sender_slot<T>(sender: oneshot::Sender<T>) -> SenderSlot<T> {
    Arc::new(Mutex::new(Some(sender)))
}

fn send_once<T>(slot: &SenderSlot<T>, value: T) {
    if let Some(sender) = slot.lock().expect("mutex not poisoned").take() {
        let _ = sender.send(value);
    }
}

#[derive(Clone)]
struct HandledBlockingFailure {
    handled: SenderSlot<()>,
}

impl Actor for HandledBlockingFailure {
    type Msg = ();

    async fn run(&self, mut ctx: ActorContext<()>) -> ActorResult {
        let handle = ctx
            .spawn_blocking(BlockingOptions::named("boom"), |_job| {
                Err(io::Error::other("boom").into())
            })
            .expect("blocking task spawned");
        handle.wait().await.expect_err("blocking task should fail");
        send_once(&self.handled, ());

        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[tokio::test]
async fn awaited_blocking_task_failures_can_be_handled_locally() {
    let (handled_tx, handled_rx) = oneshot::channel();
    let mut builder = GraphBuilder::new();
    builder.actor(
        "worker",
        HandledBlockingFailure {
            handled: sender_slot(handled_tx),
        },
    );
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    handled_rx.await.expect("actor handled blocking failure");
    stop_graph(stop, task).await;
}

#[tokio::test]
async fn dropped_graph_releases_waiting_refs() {
    let mut builder = GraphBuilder::new();
    let mut echo = builder.actor("echo", Drain::<u32>::new());
    let graph = builder.build().expect("valid graph");

    drop(graph);
    assert!(!echo.wait_for_binding().await);
}
