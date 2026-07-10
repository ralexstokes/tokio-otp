use std::{
    fmt,
    future::pending,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_actor::{
    ActorContext, ActorRef, ActorRegistry, ActorResult, ActorRunError, ActorSet, BoxError,
    GraphBuilder, LookupError, RawActor, RebindPolicy, RunnableActor, SendError,
};
use tokio_util::sync::CancellationToken;
use tracing::{Dispatch, field::Visit};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

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

impl<M: Send + 'static> RawActor for Drain<M> {
    type Msg = M;

    async fn run(&self, mut ctx: ActorContext<M>) -> ActorResult {
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[derive(Clone)]
struct NeverStops;

impl RawActor for NeverStops {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        pending::<ActorResult>().await
    }
}

#[derive(Clone)]
struct StopsOnShutdown;

impl RawActor for StopsOnShutdown {
    type Msg = ();

    async fn run(&self, ctx: ActorContext<()>) -> ActorResult {
        ctx.shutdown_token().cancelled().await;
        Ok(())
    }
}

#[derive(Clone)]
struct GatedDrain {
    started: mpsc::UnboundedSender<()>,
    release: Arc<Notify>,
    received: mpsc::UnboundedSender<()>,
}

impl RawActor for GatedDrain {
    type Msg = u32;

    async fn run(&self, mut ctx: ActorContext<u32>) -> ActorResult {
        self.started.send(()).expect("receiver alive");
        self.release.notified().await;
        while let Some(_message) = ctx.recv().await {
            self.received.send(()).expect("receiver alive");
        }
        Ok(())
    }
}

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

fn single_actor(actor_set: &ActorSet, id: &str) -> RunnableActor {
    actor_set.actor(id).expect("actor exists").clone()
}

#[tokio::test]
async fn actor_stats_track_send_receive_and_bounded_mailbox() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (received_tx, mut received_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let mut builder = GraphBuilder::new();
    builder.mailbox_capacity(2);
    let worker_ref = builder.actor(
        "worker",
        GatedDrain {
            started: started_tx,
            release: Arc::clone(&release),
            received: received_tx,
        },
    );
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");
    let worker = single_actor(&actor_set, "worker");
    let (stop, task) = start_actor(worker);

    started_rx.recv().await.expect("actor started");
    worker_ref.send(1).await.expect("send accepted");
    worker_ref.try_send(2).expect("try_send accepted");
    assert!(matches!(
        worker_ref.try_send(3),
        Err(SendError::MailboxFull { .. })
    ));

    assert_eq!(
        worker_ref.stats(),
        tokio_actor::ActorStats {
            actor_id: "worker".to_owned(),
            messages_received: 0,
            messages_accepted: 2,
            sends_rejected: 1,
            mailbox_depth: 2,
            mailbox_capacity: 2,
        }
    );
    assert_eq!(actor_set.stats(), vec![worker_ref.stats()]);

    release.notify_one();
    received_rx.recv().await.expect("first message received");
    received_rx.recv().await.expect("second message received");
    let stats = worker_ref.stats();
    assert_eq!(stats.messages_received, 2);
    assert_eq!(stats.mailbox_depth, 0);
    assert_eq!(stats.mailbox_capacity, 2);

    stop_actor(stop, task).await.expect("actor stops");
}

#[tokio::test(start_paused = true)]
async fn runnable_actor_shutdown_timeout_aborts_uncooperative_actor_cleanly() {
    let mut builder = GraphBuilder::new();
    builder.actor_shutdown_timeout(Duration::from_millis(100));
    builder.actor("worker", NeverStops);
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");
    let worker = single_actor(&actor_set, "worker");

    worker
        .run_until(async {})
        .await
        .expect("timeout abort is a clean requested shutdown");
}

#[tokio::test(start_paused = true)]
async fn runnable_actor_shutdown_timeout_leaves_cooperative_actor_clean() {
    let mut builder = GraphBuilder::new();
    builder.actor_shutdown_timeout(Duration::from_secs(30));
    builder.actor("worker", StopsOnShutdown);
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");
    let worker = single_actor(&actor_set, "worker");

    worker
        .run_until(async {})
        .await
        .expect("cooperative shutdown completes cleanly");
}

#[tokio::test(start_paused = true)]
async fn dynamic_factory_actor_inherits_shutdown_timeout() {
    let mut builder = GraphBuilder::new();
    builder.actor_shutdown_timeout(Duration::from_millis(100));
    builder.actor("anchor", Drain::<()>::new());
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");
    let worker = actor_set.dynamic_factory().actor("worker", NeverStops);

    worker
        .run_until(async {})
        .await
        .expect("factory actor uses inherited shutdown timeout");
}

#[derive(Clone, Default)]
struct MailboxClosedCounter {
    count: Arc<AtomicUsize>,
}

impl MailboxClosedCounter {
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl<S> Layer<S> for MailboxClosedCounter
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = RejectionVisitor::default();
        event.record(&mut visitor);
        if visitor.mailbox_closed {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[derive(Default)]
struct RejectionVisitor {
    mailbox_closed: bool,
}

impl Visit for RejectionVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "reason" && value == "mailbox_closed" {
            self.mailbox_closed = true;
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "reason" && format!("{value:?}").contains("mailbox_closed") {
            self.mailbox_closed = true;
        }
    }
}

fn mailbox_closed_counter_dispatch(counter: MailboxClosedCounter) -> Dispatch {
    Dispatch::new(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(counter),
    )
}

async fn wait_for_stale_mailbox(actor_ref: &ActorRef<String>) {
    timeout(Duration::from_secs(1), async {
        loop {
            match actor_ref.try_send("probe".to_owned()) {
                Err(SendError::MailboxClosed { .. }) => break,
                Err(SendError::ActorNotRunning { .. }) => {
                    panic!("binding cleared before stale mailbox was observed");
                }
                Err(SendError::ActorTerminated { .. }) => {
                    panic!("binding terminated before stale mailbox was observed");
                }
                Ok(()) | Err(SendError::MailboxFull { .. }) => {
                    sleep(Duration::from_millis(1)).await;
                }
            }
        }
    })
    .await
    .expect("stale mailbox observed in time");
}

#[derive(Clone)]
struct RebindActor {
    runs: Arc<AtomicUsize>,
    entered_stale_window: mpsc::UnboundedSender<()>,
    release_first_run: Arc<Notify>,
    observed: mpsc::UnboundedSender<String>,
}

impl RawActor for RebindActor {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        if run == 0 {
            drop(ctx);
            self.entered_stale_window.send(()).expect("receiver alive");
            self.release_first_run.notified().await;
            return Ok(());
        }

        while let Some(message) = ctx.recv().await {
            self.observed.send(message).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actor_ref_send_waits_for_stale_binding_to_change() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());

    let mut builder = GraphBuilder::new();
    builder.actor(
        "worker",
        RebindActor {
            runs: Arc::new(AtomicUsize::new(0)),
            entered_stale_window: entered_tx,
            release_first_run: release.clone(),
            observed: observed_tx,
        },
    );
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let worker = single_actor(&actor_set, "worker");
    worker.set_rebind_policy(RebindPolicy::Always);
    let actor_ref = worker.actor_ref::<String>().expect("typed ref");
    let (_first_stop, first_task) = start_actor(worker.clone());

    timeout(Duration::from_secs(1), entered_rx.recv())
        .await
        .expect("actor entered stale window")
        .expect("actor reported stale window");
    wait_for_stale_mailbox(&actor_ref).await;

    let counter = MailboxClosedCounter::default();
    let dispatch = mailbox_closed_counter_dispatch(counter.clone());
    let guard = tracing::dispatcher::set_default(&dispatch);

    let sending_ref = actor_ref.clone();
    let send_task = tokio::spawn(async move { sending_ref.send("held".to_owned()).await });
    timeout(Duration::from_secs(1), async {
        while counter.count() == 0 {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("send task observed stale closed mailbox in time");
    assert!(
        !send_task.is_finished(),
        "send should wait for a new binding"
    );

    drop(guard);
    assert_eq!(
        counter.count(),
        1,
        "stale closed mailbox should be observed once before waiting"
    );

    release.notify_one();
    first_task
        .await
        .expect("first actor task joined")
        .expect("first actor run completed cleanly");

    let (second_stop, second_task) = start_actor(worker);
    assert_eq!(
        timeout(Duration::from_secs(1), observed_rx.recv())
            .await
            .expect("held message delivered")
            .expect("message observed"),
        "held"
    );
    send_task
        .await
        .expect("send task joined")
        .expect("send completed after rebind");

    stop_actor(second_stop, second_task)
        .await
        .expect("second actor stopped cleanly");
}

#[tokio::test]
async fn runnable_actor_rejects_concurrent_runs() {
    let mut builder = GraphBuilder::new();
    builder.actor("worker", Drain::<()>::new());
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let worker = single_actor(&actor_set, "worker");
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

struct Work(&'static str);

#[derive(Clone)]
struct Forwarder {
    worker: ActorRef<Work>,
}

impl RawActor for Forwarder {
    type Msg = Work;

    async fn run(&self, mut ctx: ActorContext<Work>) -> ActorResult {
        while let Some(work) = ctx.recv().await {
            let worker = self.worker.clone();
            worker.send(work).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RestartingWorker {
    runs: Arc<AtomicUsize>,
    observed: mpsc::UnboundedSender<&'static str>,
}

impl RawActor for RestartingWorker {
    type Msg = Work;

    async fn run(&self, mut ctx: ActorContext<Work>) -> ActorResult {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        while let Some(Work(payload)) = ctx.recv().await {
            self.observed.send(payload).expect("receiver alive");
            if run == 0 {
                return Err::<(), BoxError>(std::io::Error::other("boom").into());
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn actor_set_refs_survive_individual_actor_restarts() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let (worker_slot, worker_ref) = builder.slot::<Work>("worker");
    builder.actor("frontend", Forwarder { worker: worker_ref });
    builder.define(
        worker_slot,
        RestartingWorker {
            runs: Arc::new(AtomicUsize::new(0)),
            observed: observed_tx,
        },
    );
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let frontend = single_actor(&actor_set, "frontend");
    let worker = single_actor(&actor_set, "worker");
    worker.set_rebind_policy(RebindPolicy::OnFailure);
    let frontend_ref = actor_set
        .actor_ref::<Work>("frontend")
        .expect("frontend ref exists");

    let (frontend_stop, frontend_task) = start_actor(frontend);
    let (_first_worker_stop, first_worker_task) = start_actor(worker.clone());

    frontend_ref.send(Work("first")).await.expect("first send");
    assert_eq!(
        timeout(Duration::from_secs(1), observed_rx.recv())
            .await
            .expect("first observed")
            .expect("message observed"),
        "first"
    );
    assert!(matches!(
        timeout(Duration::from_secs(1), first_worker_task)
            .await
            .expect("first worker exited")
            .expect("first worker task joined"),
        Err(ActorRunError::Failed { ref actor_id, .. }) if actor_id == "worker"
    ));

    frontend_ref
        .send(Work("second"))
        .await
        .expect("second send");
    assert!(
        timeout(Duration::from_millis(100), observed_rx.recv())
            .await
            .is_err(),
        "frontend should hold the message until worker restarts"
    );

    let (second_worker_stop, second_worker_task) = start_actor(worker);
    assert_eq!(
        timeout(Duration::from_secs(1), observed_rx.recv())
            .await
            .expect("second observed")
            .expect("message observed"),
        "second"
    );

    stop_actor(frontend_stop, frontend_task)
        .await
        .expect("frontend stopped cleanly");
    stop_actor(second_worker_stop, second_worker_task)
        .await
        .expect("worker stopped cleanly");
}

enum ProbeMsg {
    Check,
}

#[derive(Clone)]
struct RegistryProbe {
    result: mpsc::UnboundedSender<bool>,
}

impl RawActor for RegistryProbe {
    type Msg = ProbeMsg;

    async fn run(&self, mut ctx: ActorContext<ProbeMsg>) -> ActorResult {
        while let Some(ProbeMsg::Check) = ctx.recv().await {
            let mismatch = matches!(
                ctx.registry()
                    .expect("registry installed")
                    .actor_ref::<String>("numbers"),
                Err(LookupError::MessageTypeMismatch { .. })
            );
            self.result.send(mismatch).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn context_registry_lookup_checks_message_type() {
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    builder.actor("numbers", Drain::<u32>::new());
    builder.actor("probe", RegistryProbe { result: result_tx });
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");

    let registry = ActorRegistry::new();
    for actor in actor_set.actors() {
        actor.set_registry(registry.clone());
        actor.register_with(&registry).expect("actor registers");
    }

    let probe = single_actor(&actor_set, "probe");
    let probe_ref = actor_set.actor_ref::<ProbeMsg>("probe").expect("probe ref");
    let (stop, task) = start_actor(probe);

    probe_ref.send(ProbeMsg::Check).await.expect("probe send");
    assert!(
        timeout(Duration::from_secs(1), result_rx.recv())
            .await
            .expect("probe answered")
            .expect("probe result")
    );

    stop_actor(stop, task).await.expect("probe stopped cleanly");
}

#[derive(Clone)]
struct CleanExit;

impl RawActor for CleanExit {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

#[tokio::test]
async fn registry_evicts_actor_after_terminal_clean_exit() {
    let mut builder = GraphBuilder::new();
    builder.actor("temporary", CleanExit);
    let actor_set = builder
        .build()
        .expect("valid graph")
        .into_actor_set()
        .expect("actor set");
    let actor = single_actor(&actor_set, "temporary");
    actor.set_rebind_policy(RebindPolicy::OnFailure);

    let registry = ActorRegistry::new();
    actor.set_registry(registry.clone());
    actor.register_with(&registry).expect("actor registered");
    assert!(registry.contains("temporary"));

    let task = tokio::spawn(async move { actor.run_until(pending::<()>()).await });
    task.await
        .expect("actor task joined")
        .expect("actor exited cleanly");

    assert!(!registry.contains("temporary"));
    assert!(registry.actor_ids().is_empty());
    assert!(matches!(
        registry.actor_ref::<()>("temporary"),
        Err(LookupError::UnknownActor { actor_id }) if actor_id == "temporary"
    ));
}
