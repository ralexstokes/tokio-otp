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
    time::{sleep, timeout},
};
use tokio_actor::{
    Actor, ActorContext, ActorRef, ActorResult, BlockingOptions, BlockingTaskFailure, CallError,
    DrainPolicy, Graph, GraphBuildError, GraphBuilder, GraphError, LookupError, MessageHandler,
    Reply, SendError, TryRecvError,
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
    let (worker_slot, worker) = builder.slot::<Job>("worker");
    let frontend = builder.actor("frontend", Frontend { worker });
    builder.define(worker_slot, Worker { seen: seen_tx });
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
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
    let echo = builder.actor("echo", Echo { seen: seen_tx });
    let graph = builder.build().expect("valid graph");

    let handle = graph.spawn().expect("graph spawned");
    echo.send(1).await.expect("send during first run");
    assert_eq!(recv(&mut seen_rx, "first message").await, 1);
    handle
        .shutdown_and_wait()
        .await
        .expect("graph stopped cleanly");

    assert!(matches!(
        echo.try_send(2),
        Err(SendError::ActorTerminated { .. })
    ));

    let handle = graph.spawn().expect("graph spawned");
    echo.send(3).await.expect("send across rerun");
    assert_eq!(recv(&mut seen_rx, "second message").await, 3);
    handle
        .shutdown_and_wait()
        .await
        .expect("graph stopped cleanly");
}

#[tokio::test]
async fn send_to_never_started_graph_waits_until_graph_runs() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let echo = builder.actor("echo", Echo { seen: seen_tx });
    let graph = builder.build().expect("valid graph");

    let send_task = tokio::spawn({
        let echo = echo.clone();
        async move { echo.send(7).await }
    });
    sleep(Duration::from_millis(25)).await;
    assert!(
        !send_task.is_finished(),
        "send should wait until the graph binds the actor mailbox"
    );

    let (stop, task) = start_graph(&graph);
    assert_eq!(recv(&mut seen_rx, "message after graph start").await, 7);
    send_task
        .await
        .expect("send task joined")
        .expect("send completed after graph start");
    stop_graph(stop, task).await;
}

#[tokio::test]
async fn try_send_reports_unbound_and_terminated_states() {
    let mut builder = GraphBuilder::new();
    let worker = builder.actor("worker", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    assert!(matches!(
        worker.try_send(()),
        Err(SendError::ActorNotRunning { actor_id }) if actor_id == "worker"
    ));

    let (stop, task) = start_graph(&graph);
    stop_graph(stop, task).await;

    assert!(matches!(
        worker.try_send(()),
        Err(SendError::ActorTerminated { actor_id }) if actor_id == "worker"
    ));
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
    let counter = builder.actor("counter", Counter);
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    counter.send(CounterMsg::Add(1)).await.expect("send");
    counter.send(CounterMsg::Add(2)).await.expect("send");

    assert_eq!(counter.call(CounterMsg::Total).await.expect("call"), 3);

    stop_graph(stop, task).await;
}

enum HandlerCounterMsg {
    Add(u64),
    Total(Reply<u64>),
}

#[derive(Clone)]
struct HandlerCounter {
    total: u64,
}

impl MessageHandler for HandlerCounter {
    type Msg = HandlerCounterMsg;

    async fn handle(
        &mut self,
        message: HandlerCounterMsg,
        _ctx: &ActorContext<HandlerCounterMsg>,
    ) -> ActorResult {
        match message {
            HandlerCounterMsg::Add(n) => self.total += n,
            HandlerCounterMsg::Total(reply) => reply.send(self.total),
        }
        Ok(())
    }
}

#[tokio::test]
async fn handler_receives_messages_in_order_and_preserves_state() {
    let mut builder = GraphBuilder::new();
    let counter = builder.actor("counter", HandlerCounter { total: 0 });
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    counter
        .send(HandlerCounterMsg::Add(2))
        .await
        .expect("first add sent");
    counter
        .send(HandlerCounterMsg::Add(3))
        .await
        .expect("second add sent");

    assert_eq!(
        counter.call(HandlerCounterMsg::Total).await.expect("call"),
        5
    );

    stop_graph(stop, task).await;
}

#[tokio::test]
async fn handler_state_resets_on_graph_rerun_and_ref_rebinds() {
    let mut builder = GraphBuilder::new();
    let counter = builder.actor("counter", HandlerCounter { total: 0 });
    let graph = builder.build().expect("valid graph");

    let handle = graph.spawn().expect("graph spawned");
    counter
        .send(HandlerCounterMsg::Add(5))
        .await
        .expect("first run add sent");
    assert_eq!(
        counter
            .call(HandlerCounterMsg::Total)
            .await
            .expect("first total"),
        5
    );
    handle
        .shutdown_and_wait()
        .await
        .expect("graph stopped cleanly");

    let handle = graph.spawn().expect("graph spawned");
    counter
        .send(HandlerCounterMsg::Add(1))
        .await
        .expect("send across rerun");
    assert_eq!(
        counter
            .call(HandlerCounterMsg::Total)
            .await
            .expect("second total"),
        1
    );
    handle
        .shutdown_and_wait()
        .await
        .expect("graph stopped cleanly");
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LifecycleEvent {
    Started,
    Handled,
    Stopped,
}

#[derive(Clone)]
struct LifecycleHandler {
    events: mpsc::UnboundedSender<LifecycleEvent>,
}

impl MessageHandler for LifecycleHandler {
    type Msg = ();

    async fn on_start(&mut self, _ctx: &ActorContext<()>) -> ActorResult {
        self.events
            .send(LifecycleEvent::Started)
            .expect("receiver alive");
        Ok(())
    }

    async fn handle(&mut self, _message: (), _ctx: &ActorContext<()>) -> ActorResult {
        self.events
            .send(LifecycleEvent::Handled)
            .expect("receiver alive");
        Ok(())
    }

    async fn on_stop(&mut self, _ctx: &ActorContext<()>) -> ActorResult {
        self.events
            .send(LifecycleEvent::Stopped)
            .expect("receiver alive");
        Ok(())
    }
}

#[tokio::test]
async fn handler_on_start_runs_before_first_message() {
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let actor = builder.actor("worker", LifecycleHandler { events: events_tx });
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    assert_eq!(
        recv(&mut events_rx, "handler started").await,
        LifecycleEvent::Started
    );

    actor.send(()).await.expect("message sent");
    assert_eq!(
        recv(&mut events_rx, "handler handled message").await,
        LifecycleEvent::Handled
    );

    stop_graph(stop, task).await;
    assert_eq!(
        recv(&mut events_rx, "handler stopped").await,
        LifecycleEvent::Stopped
    );
}

#[derive(Clone)]
struct FailingStartHandler {
    events: mpsc::UnboundedSender<LifecycleEvent>,
}

impl MessageHandler for FailingStartHandler {
    type Msg = ();

    async fn on_start(&mut self, _ctx: &ActorContext<()>) -> ActorResult {
        self.events
            .send(LifecycleEvent::Started)
            .expect("receiver alive");
        Err(io::Error::other("start failed").into())
    }

    async fn handle(&mut self, _message: (), _ctx: &ActorContext<()>) -> ActorResult {
        self.events
            .send(LifecycleEvent::Handled)
            .expect("receiver alive");
        Ok(())
    }

    async fn on_stop(&mut self, _ctx: &ActorContext<()>) -> ActorResult {
        self.events
            .send(LifecycleEvent::Stopped)
            .expect("receiver alive");
        Ok(())
    }
}

#[tokio::test]
async fn handler_on_start_error_fails_without_handle_or_stop() {
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    builder.actor("worker", FailingStartHandler { events: events_tx });
    let graph = builder.build().expect("valid graph");

    let result = graph.run_until(pending::<()>()).await;
    assert!(matches!(
        result,
        Err(GraphError::ActorFailed { actor_id, .. }) if actor_id == "worker"
    ));
    assert_eq!(
        recv(&mut events_rx, "handler started").await,
        LifecycleEvent::Started
    );
    assert!(matches!(
        events_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[derive(Clone)]
struct FailingHandler;

impl MessageHandler for FailingHandler {
    type Msg = ();

    async fn handle(&mut self, _message: (), _ctx: &ActorContext<()>) -> ActorResult {
        Err(io::Error::other("handle failed").into())
    }
}

#[tokio::test]
async fn handler_error_fails_the_graph() {
    let mut builder = GraphBuilder::new();
    let actor = builder.actor("worker", FailingHandler);
    let graph = builder.build().expect("valid graph");

    let (_stop, task) = start_graph(&graph);
    actor.send(()).await.expect("message sent");

    let result = timeout(Duration::from_secs(1), task)
        .await
        .expect("graph stopped in time")
        .expect("graph task joined");
    assert!(matches!(
        result,
        Err(GraphError::ActorFailed { actor_id, .. }) if actor_id == "worker"
    ));
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GateEvent {
    Handled(u32),
    Stopped(u32),
}

enum GateMsg {
    Hold,
    Add(u32),
    Total(Reply<u32>),
}

#[derive(Clone)]
struct GateHandler {
    total: u32,
    policy: DrainPolicy,
    started: mpsc::UnboundedSender<()>,
    release: Arc<Notify>,
    events: mpsc::UnboundedSender<GateEvent>,
}

impl MessageHandler for GateHandler {
    type Msg = GateMsg;

    async fn handle(&mut self, message: GateMsg, _ctx: &ActorContext<GateMsg>) -> ActorResult {
        match message {
            GateMsg::Hold => {
                self.started.send(()).expect("receiver alive");
                self.release.notified().await;
            }
            GateMsg::Add(n) => {
                self.total += n;
                self.events
                    .send(GateEvent::Handled(n))
                    .expect("receiver alive");
            }
            GateMsg::Total(reply) => reply.send(self.total),
        }
        Ok(())
    }

    async fn on_stop(&mut self, _ctx: &ActorContext<GateMsg>) -> ActorResult {
        self.events
            .send(GateEvent::Stopped(self.total))
            .expect("receiver alive");
        Ok(())
    }

    fn drain_policy(&self) -> DrainPolicy {
        self.policy
    }
}

async fn queued_total_call(actor: ActorRef<GateMsg>) -> JoinHandle<Result<u32, CallError>> {
    let (queued_tx, queued_rx) = oneshot::channel();
    let call_task = tokio::spawn(async move {
        actor
            .call(|reply| {
                queued_tx.send(()).expect("receiver alive");
                GateMsg::Total(reply)
            })
            .await
    });
    queued_rx.await.expect("call message constructed");
    call_task
}

#[tokio::test]
async fn handler_discard_drops_queued_messages_and_call_reply() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());

    let mut builder = GraphBuilder::new();
    let actor = builder.actor(
        "worker",
        GateHandler {
            total: 0,
            policy: DrainPolicy::Discard,
            started: started_tx,
            release: release.clone(),
            events: events_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    actor.send(GateMsg::Hold).await.expect("hold sent");
    recv(&mut started_rx, "handler entered hold").await;

    actor.send(GateMsg::Add(1)).await.expect("add queued");
    let call_task = queued_total_call(actor.clone()).await;

    stop.cancel();
    release.notify_one();
    timeout(Duration::from_secs(1), task)
        .await
        .expect("graph stopped in time")
        .expect("graph task joined")
        .expect("graph stopped cleanly");

    assert!(matches!(
        call_task.await.expect("call task joined"),
        Err(CallError::ReplyDropped { actor_id }) if actor_id == "worker"
    ));
    assert_eq!(
        recv(&mut events_rx, "handler stopped").await,
        GateEvent::Stopped(0)
    );
    assert!(matches!(
        events_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn handler_drain_handles_queued_messages_and_replies_before_stop() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());

    let mut builder = GraphBuilder::new();
    let actor = builder.actor(
        "worker",
        GateHandler {
            total: 0,
            policy: DrainPolicy::Drain,
            started: started_tx,
            release: release.clone(),
            events: events_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    actor.send(GateMsg::Hold).await.expect("hold sent");
    recv(&mut started_rx, "handler entered hold").await;

    actor.send(GateMsg::Add(1)).await.expect("first add queued");
    actor
        .send(GateMsg::Add(2))
        .await
        .expect("second add queued");
    let call_task = queued_total_call(actor.clone()).await;

    stop.cancel();
    release.notify_one();

    assert_eq!(
        call_task.await.expect("call task joined").expect("reply"),
        3
    );
    timeout(Duration::from_secs(1), task)
        .await
        .expect("graph stopped in time")
        .expect("graph task joined")
        .expect("graph stopped cleanly");

    assert_eq!(
        recv(&mut events_rx, "first drained message").await,
        GateEvent::Handled(1)
    );
    assert_eq!(
        recv(&mut events_rx, "second drained message").await,
        GateEvent::Handled(2)
    );
    assert_eq!(
        recv(&mut events_rx, "handler stopped").await,
        GateEvent::Stopped(3)
    );
}

enum TryDrainMsg {
    Start,
    Value(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TryDrainEvent {
    Drained(u32),
    Empty,
}

#[derive(Clone)]
struct TryDrainActor {
    started: mpsc::UnboundedSender<()>,
    release: Arc<Notify>,
    events: mpsc::UnboundedSender<TryDrainEvent>,
}

impl Actor for TryDrainActor {
    type Msg = TryDrainMsg;

    async fn run(&self, mut ctx: ActorContext<TryDrainMsg>) -> ActorResult {
        match ctx.recv().await {
            Some(TryDrainMsg::Start) => self.started.send(()).expect("receiver alive"),
            Some(TryDrainMsg::Value(_)) => panic!("expected start message"),
            None => panic!("shutdown before start"),
        }

        self.release.notified().await;
        assert!(ctx.recv().await.is_none());

        loop {
            match ctx.try_recv() {
                Ok(TryDrainMsg::Value(value)) => self
                    .events
                    .send(TryDrainEvent::Drained(value))
                    .expect("receiver alive"),
                Ok(TryDrainMsg::Start) => panic!("unexpected start message"),
                Err(TryRecvError::Empty) => {
                    self.events
                        .send(TryDrainEvent::Empty)
                        .expect("receiver alive");
                    break;
                }
                Err(TryRecvError::Disconnected) => panic!("mailbox disconnected"),
            }
        }

        Ok(())
    }
}

#[tokio::test]
async fn try_recv_drains_messages_after_shutdown_recv_returns_none() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());

    let mut builder = GraphBuilder::new();
    let actor = builder.actor(
        "worker",
        TryDrainActor {
            started: started_tx,
            release: release.clone(),
            events: events_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
    actor.send(TryDrainMsg::Start).await.expect("start sent");
    recv(&mut started_rx, "actor started").await;
    actor
        .send(TryDrainMsg::Value(1))
        .await
        .expect("first value queued");
    actor
        .send(TryDrainMsg::Value(2))
        .await
        .expect("second value queued");

    stop.cancel();
    release.notify_one();
    timeout(Duration::from_secs(1), task)
        .await
        .expect("graph stopped in time")
        .expect("graph task joined")
        .expect("graph stopped cleanly");

    assert_eq!(
        recv(&mut events_rx, "first drained value").await,
        TryDrainEvent::Drained(1)
    );
    assert_eq!(
        recv(&mut events_rx, "second drained value").await,
        TryDrainEvent::Drained(2)
    );
    assert_eq!(
        recv(&mut events_rx, "empty mailbox observed").await,
        TryDrainEvent::Empty
    );
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
async fn cyclic_wiring_via_slot() {
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let (pong_slot, pong) = builder.slot::<Ball>("pong");
    let ping = builder.actor(
        "ping",
        Paddle {
            other: pong,
            done: done_tx.clone(),
        },
    );
    builder.define(
        pong_slot,
        Paddle {
            other: ping.clone(),
            done: done_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let (stop, task) = start_graph(&graph);
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
    let mut missing = GraphBuilder::new();
    let (_slot, _ghost) = missing.slot::<u32>("ghost");
    assert!(matches!(
        missing.build(),
        Err(GraphBuildError::MissingActor { actor_id }) if actor_id == "ghost"
    ));

    let mut duplicate = GraphBuilder::new();
    duplicate.actor("worker", Drain::<u32>::new());
    duplicate.actor("worker", Drain::<u32>::new());
    assert!(matches!(
        duplicate.build(),
        Err(GraphBuildError::DuplicateActorId { actor_id }) if actor_id == "worker"
    ));

    let empty = GraphBuilder::new();
    assert!(matches!(empty.build(), Err(GraphBuildError::EmptyGraph)));

    let mut empty_name = GraphBuilder::new();
    empty_name.name("");
    empty_name.actor("worker", Drain::<()>::new());
    assert!(matches!(
        empty_name.build(),
        Err(GraphBuildError::InvalidConfig(
            "graph name must not be empty"
        ))
    ));

    let mut zero_capacity = GraphBuilder::new();
    zero_capacity.mailbox_capacity(0);
    zero_capacity.actor("worker", Drain::<()>::new());
    assert!(matches!(
        zero_capacity.build(),
        Err(GraphBuildError::InvalidConfig(
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
    builder.actor("worker", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let handle = graph.spawn().expect("graph spawned");
    assert!(matches!(
        graph.run_until(async {}).await,
        Err(GraphError::AlreadyRunning)
    ));
    handle
        .shutdown_and_wait()
        .await
        .expect("graph stopped cleanly");
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
async fn send_to_dropped_never_started_graph_returns_actor_terminated() {
    let mut builder = GraphBuilder::new();
    let echo = builder.actor("echo", Drain::<u32>::new());
    let graph = builder.build().expect("valid graph");

    drop(graph);
    assert!(matches!(
        echo.send(1).await,
        Err(SendError::ActorTerminated { actor_id }) if actor_id == "echo"
    ));
}
