use std::{
    future::IntoFuture,
    io,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{sync::mpsc, time::timeout};
use tokio_otp::{
    Actor, ActorContext, ActorRef, ActorResult, BoxError, DynamicActorOptions, GraphBuilder,
    RawActor, Reply, Runtime, SendError, SupervisedActors, SupervisorHandleExt,
};
use tokio_supervisor::{
    ChildStateView, ExitStatusView, RestartIntensity, RestartPolicy, Strategy, SupervisorBuilder,
    SupervisorStateView,
};

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
struct Observe {
    observed: mpsc::UnboundedSender<String>,
}

impl RawActor for Observe {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            self.observed.send(message).expect("receiver alive");
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ObserveOnce {
    observed: mpsc::UnboundedSender<String>,
}

impl RawActor for ObserveOnce {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let message = ctx.recv().await.expect("message received before shutdown");
        self.observed.send(message).expect("receiver alive");
        Ok(())
    }
}

fn build_runtime<A>(actor: A) -> (Runtime, ActorRef<A::Msg>)
where
    A: RawActor,
{
    let mut builder = GraphBuilder::new();
    let actor_ref = builder.actor("worker", actor);
    let graph = builder.build().expect("valid graph");

    let runtime = SupervisedActors::new(graph)
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("runtime builds");

    (runtime, actor_ref)
}

#[tokio::test]
async fn runtime_spawn_combines_actor_refs_and_supervisor_control() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, worker_ref) = build_runtime(Observe {
        observed: observed_tx,
    });

    let handle = runtime.spawn();
    let supervisor_handle = handle.supervisor_handle();

    assert_eq!(
        supervisor_handle.snapshot().state,
        SupervisorStateView::Running
    );
    assert_eq!(handle.snapshot().children.len(), 1);
    worker_ref
        .send("hello".to_owned())
        .await
        .expect("message sent");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker observed the message")
        .expect("worker is still running");
    assert_eq!(observed, "hello");

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}

#[tokio::test]
async fn runtime_handle_enumerates_actor_stats() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, worker_ref) = build_runtime(Observe {
        observed: observed_tx,
    });
    let handle = runtime.spawn();

    worker_ref
        .send("counted".to_owned())
        .await
        .expect("message sent");
    observed_rx.recv().await.expect("message received");

    let stats = handle.actor_stats();
    assert_eq!(stats, vec![worker_ref.stats()]);
    assert_eq!(stats[0].messages_accepted, 1);
    assert_eq!(stats[0].messages_received, 1);

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}

#[derive(Clone)]
struct FailAfterObserve {
    observed: mpsc::UnboundedSender<String>,
}

impl RawActor for FailAfterObserve {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        match ctx.recv().await {
            Some(message) => {
                self.observed.send(message).expect("receiver alive");
                Err("deliberate failure".into())
            }
            None => Ok(()),
        }
    }
}

#[tokio::test]
async fn actor_stats_accumulate_across_supervised_restarts() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, worker_ref) = build_runtime(FailAfterObserve {
        observed: observed_tx,
    });
    let handle = runtime.spawn();

    worker_ref
        .send("first".to_owned())
        .await
        .expect("first message sent");
    observed_rx.recv().await.expect("first message observed");

    // The incarnation fails after observing; `send` waits for the restarted
    // incarnation's mailbox to bind before delivering.
    timeout(Duration::from_secs(5), worker_ref.send("second".to_owned()))
        .await
        .expect("rebind within timeout")
        .expect("second message sent");
    observed_rx.recv().await.expect("second message observed");

    let restarted = handle
        .snapshot()
        .child("worker")
        .map_or(0, |child| child.restart_count);
    assert!(restarted >= 1, "worker should have restarted");

    // Counters accumulate across incarnations; mailbox fields describe only
    // the current binding (and are zero in the window between incarnations),
    // so only the counters are asserted here.
    let stats = worker_ref.stats();
    assert_eq!(stats.messages_received, 2);
    assert_eq!(stats.messages_accepted, 2);
    assert_eq!(stats.sends_rejected, 0);

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}

#[tokio::test]
async fn runtime_into_supervisor_spawn_accepts_ref_cloned_before_startup() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, worker_ref) = build_runtime(ObserveOnce {
        observed: observed_tx,
    });

    let handle = runtime.into_supervisor().spawn();
    let mut snapshots = handle.subscribe_snapshots();
    let sender = tokio::spawn(async move {
        worker_ref
            .send("run-path".to_owned())
            .await
            .expect("message sent through cloned ref");
    });

    sender.await.expect("sender task joined");

    let completed = snapshots
        .wait_for(|snapshot| {
            snapshot
                .child("worker")
                .is_some_and(|child| child.state == ChildStateView::Stopped)
        })
        .await
        .expect("completion snapshot remains available")
        .clone();
    assert!(matches!(
        completed
            .child("worker")
            .expect("worker remains visible")
            .last_exit
            .as_ref(),
        Some(ExitStatusView::Completed)
    ));

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker observed the message")
        .expect("worker is still running");
    assert_eq!(observed, "run-path");
    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn runtime_spawn_wait_drives_to_completion_with_control_surface() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, worker_ref) = build_runtime(ObserveOnce {
        observed: observed_tx,
    });

    let handle = runtime.spawn();
    let control = handle.clone();

    control
        .subscribe_snapshots()
        .wait_for(|snapshot| {
            snapshot
                .children
                .iter()
                .all(|child| child.state == ChildStateView::Running)
        })
        .await
        .expect("runtime reported running");
    let _events = control.subscribe();
    assert_eq!(control.snapshot().children.len(), 1);

    worker_ref
        .send("spawn-wait-path".to_owned())
        .await
        .expect("message sent through cloned ref");

    let mut snapshots = control.subscribe_snapshots();
    let completed = snapshots
        .wait_for(|snapshot| {
            snapshot
                .child("worker")
                .is_some_and(|child| child.state == ChildStateView::Stopped)
        })
        .await
        .expect("completion snapshot remains available")
        .clone();
    assert!(matches!(
        completed
            .child("worker")
            .expect("worker remains visible")
            .last_exit
            .as_ref(),
        Some(ExitStatusView::Completed)
    ));

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker observed the message")
        .expect("worker is still running");
    assert_eq!(observed, "spawn-wait-path");
    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn runtime_builder_wires_graph_into_supervised_runtime() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let worker_ref = builder.actor(
        "worker",
        Observe {
            observed: observed_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let runtime = Runtime::builder()
        .graph(graph)
        .strategy(Strategy::OneForOne)
        .build()
        .expect("runtime builds");
    let handle = runtime.spawn();

    worker_ref
        .send("built".to_owned())
        .await
        .expect("message sent");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker observed the message")
        .expect("worker is still running");
    assert_eq!(observed, "built");

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}

#[tokio::test]
async fn snapshot_wait_reports_all_children_running_after_spawn() {
    let mut builder = GraphBuilder::new();
    builder.actor("one", Drain::<()>::new());
    builder.actor("two", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let runtime = SupervisedActors::new(graph)
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("runtime builds");
    let handle = runtime.spawn();

    let mut snapshots = handle.subscribe_snapshots();
    let all_running = snapshots.wait_for(|snapshot| {
        snapshot.children.len() == 2
            && snapshot
                .children
                .iter()
                .all(|child| child.state == ChildStateView::Running)
    });
    timeout(Duration::from_secs(1), all_running)
        .await
        .expect("runtime reported running")
        .expect("snapshot channel stays open");
    assert_eq!(handle.snapshot().children.len(), 2);

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime shut down cleanly");
}

#[derive(Clone)]
struct FailOnMessage;

impl RawActor for FailOnMessage {
    type Msg = ();

    async fn run(&self, mut ctx: ActorContext<()>) -> ActorResult {
        if ctx.recv().await.is_some() {
            return Err::<(), BoxError>(Box::new(io::Error::other("boom")));
        }

        Ok(())
    }
}

#[tokio::test]
async fn runtime_handle_monitor_restart_delegates_to_supervisor() {
    let mut builder = GraphBuilder::new();
    let worker_ref = builder.actor("worker", FailOnMessage);
    let graph = builder.build().expect("valid graph");

    let runtime = Runtime::builder()
        .graph(graph)
        .strategy(Strategy::OneForOne)
        .restart(RestartPolicy::OnFailure)
        .build()
        .expect("runtime builds");
    let handle = runtime.spawn();

    let restart = handle
        .monitor_restart("worker")
        .expect("worker child should be known");
    worker_ref.send(()).await.expect("message sent");

    let generation = timeout(Duration::from_secs(1), restart.into_future())
        .await
        .expect("restart monitor should resolve")
        .expect("restart monitor should succeed");
    assert_eq!(generation, 1);

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime shut down cleanly");
}

#[tokio::test]
async fn nested_supervisor_handle_adds_and_restarts_runnable_actor() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut graph_builder = GraphBuilder::new();
    graph_builder.actor("factory-anchor", Drain::<()>::new());
    let graph = graph_builder.build().expect("graph is valid");
    let (actor, actor_ref) = graph.dynamic_factory().actor(
        "subscription",
        FailAfterObserve {
            observed: observed_tx,
        },
    );

    let nested = SupervisorBuilder::new()
        .build()
        .expect("empty nested supervisor is valid");
    let outer = SupervisorBuilder::new()
        .supervisor("venue", nested)
        .build()
        .expect("outer supervisor is valid");
    let handle = outer.spawn();
    timeout(
        Duration::from_secs(1),
        handle.subscribe_snapshots().wait_for(|snapshot| {
            snapshot
                .child("venue")
                .and_then(|child| child.supervisor.as_ref())
                .is_some_and(|supervisor| supervisor.state == SupervisorStateView::Running)
        }),
    )
    .await
    .expect("nested supervisor started within timeout")
    .expect("snapshot channel remains open");
    let venue = handle
        .supervisor("venue")
        .expect("nested supervisor handle is available");

    venue
        .add_actor(actor, DynamicActorOptions::default())
        .await
        .expect("actor added to nested supervisor");
    let restart = venue
        .monitor_restart("subscription")
        .expect("actor child is known to nested supervisor");

    actor_ref
        .send("first".to_owned())
        .await
        .expect("first message sent");
    assert_eq!(observed_rx.recv().await.as_deref(), Some("first"));
    timeout(Duration::from_secs(1), restart.into_future())
        .await
        .expect("actor restarted within timeout")
        .expect("restart monitor succeeded");

    actor_ref
        .send("second".to_owned())
        .await
        .expect("ref rebound after restart");
    assert_eq!(observed_rx.recv().await.as_deref(), Some("second"));

    venue
        .remove_child("subscription")
        .await
        .expect("actor removed from nested supervisor");
    assert!(matches!(
        actor_ref.send("after removal".to_owned()).await,
        Err(SendError::ActorTerminated { .. })
    ));

    handle
        .shutdown_and_wait()
        .await
        .expect("outer supervisor shut down cleanly");
}

#[derive(Clone)]
struct AlwaysFails;

impl RawActor for AlwaysFails {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        Err::<(), BoxError>(Box::new(io::Error::other("boom")))
    }
}

#[tokio::test]
async fn send_fails_after_restart_intensity_is_exhausted() {
    let mut builder = GraphBuilder::new();
    let worker_ref = builder.actor("worker", AlwaysFails);
    let graph = builder.build().expect("valid graph");

    let runtime = Runtime::builder()
        .graph(graph)
        .strategy(Strategy::OneForOne)
        .restart(RestartPolicy::Always)
        .restart_intensity(RestartIntensity::new(1, Duration::from_secs(60)))
        .build()
        .expect("runtime builds");
    let handle = runtime.spawn();

    // The crash loop exhausts the restart budget and the supervisor gives up.
    let _ = timeout(Duration::from_secs(2), handle.wait())
        .await
        .expect("supervisor gave up");

    // A rebind will never come; send must not wait for one.
    let result = timeout(Duration::from_millis(500), worker_ref.send(()))
        .await
        .expect("send resolved after the supervisor gave up");
    assert!(matches!(result, Err(SendError::ActorTerminated { .. })));
}

enum CounterMsg {
    Add(u32),
    Total(Reply<u32>),
    Crash,
}

#[derive(Clone)]
struct ResettingCounter {
    total: u32,
    on_starts: Arc<AtomicUsize>,
}

impl Actor for ResettingCounter {
    type Msg = CounterMsg;

    async fn on_start(&mut self, _ctx: &ActorContext<CounterMsg>) -> ActorResult {
        self.on_starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(
        &mut self,
        message: CounterMsg,
        _ctx: &ActorContext<CounterMsg>,
    ) -> ActorResult {
        match message {
            CounterMsg::Add(n) => {
                self.total += n;
                Ok(())
            }
            CounterMsg::Total(reply) => {
                reply.send(self.total);
                Ok(())
            }
            CounterMsg::Crash => Err("deliberate crash".into()),
        }
    }
}

/// D10: `Clone` is the reset mechanism — a supervised restart clones the
/// wiring-time actor value, so the new incarnation starts from initial
/// state, and `on_start` runs once per incarnation.
#[tokio::test]
async fn supervised_restart_resets_actor_state_to_the_wiring_time_value() {
    let on_starts = Arc::new(AtomicUsize::new(0));
    let mut builder = GraphBuilder::new();
    let counter = builder.actor(
        "counter",
        ResettingCounter {
            total: 0,
            on_starts: Arc::clone(&on_starts),
        },
    );
    let graph = builder.build().expect("valid graph");

    let runtime = Runtime::builder()
        .graph(graph)
        .restart(RestartPolicy::Always)
        .build()
        .expect("runtime builds");
    let handle = runtime.spawn();

    counter.send(CounterMsg::Add(5)).await.expect("add sent");
    assert_eq!(
        timeout(Duration::from_secs(1), counter.call(CounterMsg::Total))
            .await
            .expect("total resolved")
            .expect("total replied"),
        5
    );

    let restart = handle
        .monitor_restart("counter")
        .expect("counter child should be known");
    counter.send(CounterMsg::Crash).await.expect("crash sent");
    timeout(Duration::from_secs(2), restart.into_future())
        .await
        .expect("restart observed")
        .expect("restart monitor succeeded");

    assert_eq!(
        timeout(Duration::from_secs(2), counter.call(CounterMsg::Total))
            .await
            .expect("total resolved after restart")
            .expect("total replied after restart"),
        0,
        "restart resets state to the wiring-time value"
    );
    assert_eq!(
        on_starts.load(Ordering::SeqCst),
        2,
        "on_start runs once per incarnation"
    );

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}

#[test]
fn runtime_builder_allows_an_empty_runtime() {
    Runtime::builder().build().expect("empty runtime builds");
}

#[tokio::test]
async fn runtime_into_supervisor_round_trips_supervisor() {
    let (runtime, _worker_ref) = build_runtime(Drain::<()>::new());

    let supervisor = runtime.into_supervisor();
    let runtime = Runtime::new(supervisor);
    let handle = runtime.spawn();

    timeout(Duration::from_secs(1), handle.shutdown_and_wait())
        .await
        .expect("shutdown completed")
        .expect("supervisor shut down cleanly");
}

#[tokio::test]
async fn handle_actor_stats_track_graph_and_runtime_added_actors() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, worker_ref) = build_runtime(Observe {
        observed: observed_tx,
    });
    let handle = runtime.spawn();

    worker_ref
        .send("count me".to_owned())
        .await
        .expect("message sent");
    observed_rx.recv().await.expect("message observed");

    // Graph actors are visible in the runtime's stats without any dynamic
    // actor having been added.
    let stats = handle.actor_stats();
    let worker = stats
        .iter()
        .find(|stats| stats.actor_id == "worker")
        .expect("graph actor reported in runtime stats");
    assert_eq!(worker.messages_accepted, 1);

    let extra = handle
        .add_actor("extra", Drain::<()>::new(), DynamicActorOptions::default())
        .await
        .expect("actor added");
    extra.send(()).await.expect("message sent");

    let stats = handle.actor_stats();
    assert_eq!(stats.len(), 2);
    let extra_stats = stats
        .iter()
        .find(|stats| stats.actor_id == "extra")
        .expect("runtime-added actor reported in runtime stats");
    assert_eq!(extra_stats.messages_accepted, 1);

    handle.remove_child("extra").await.expect("actor removed");
    let stats = handle.actor_stats();
    assert!(
        stats.iter().all(|stats| stats.actor_id != "extra"),
        "removed actor no longer reported"
    );

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}
