use std::{future::IntoFuture, io, marker::PhantomData, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{
    ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder, LookupError, RawActor, SendError,
};
use tokio_otp::{DynamicActorError, Runtime, RuntimeBuildError, SupervisedActors};
use tokio_supervisor::{
    ChildStateView, ExitStatusView, Restart, RestartIntensity, Strategy, SupervisorBuilder,
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
    assert_eq!(
        handle
            .actor_ref::<String>("worker")
            .expect("registry ref exists")
            .id(),
        "worker"
    );

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
async fn runtime_handle_reports_unknown_actor_lookup() {
    let (runtime, _worker_ref) = build_runtime(Drain::<()>::new());

    let handle = runtime.spawn();
    assert!(matches!(
        handle.actor_ref::<()>("missing"),
        Err(DynamicActorError::Lookup(LookupError::UnknownActor { .. }))
    ));

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
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

    // The builder path enables the dynamic registry.
    assert_eq!(
        handle
            .actor_ref::<String>("worker")
            .expect("registry ref exists")
            .id(),
        "worker"
    );

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
        .restart(Restart::Transient)
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
        .restart(Restart::Permanent)
        .restart_intensity(RestartIntensity::new(1, Duration::from_secs(60)))
        .build()
        .expect("runtime builds");
    let handle = runtime.spawn();

    // The crash loop exhausts the restart budget and the supervisor gives up.
    let _ = timeout(Duration::from_secs(2), handle.wait())
        .await
        .expect("supervisor gave up");

    // The handle (and its registry) is still alive, so the binding source has
    // not been dropped. A rebind will never come; send must not wait for one.
    let result = timeout(Duration::from_millis(500), worker_ref.send(()))
        .await
        .expect("send resolved after the supervisor gave up");
    assert!(matches!(result, Err(SendError::ActorTerminated { .. })));
}

#[test]
fn runtime_builder_requires_a_graph() {
    assert!(matches!(
        Runtime::builder().build(),
        Err(RuntimeBuildError::MissingGraph)
    ));
}

#[tokio::test]
async fn runtime_into_supervisor_round_trips_supervisor() {
    let (runtime, _worker_ref) = build_runtime(Drain::<()>::new());

    let supervisor = runtime.into_supervisor();
    let runtime = Runtime::new(supervisor);
    let handle = runtime.spawn();

    assert!(matches!(
        handle.actor_ref::<()>("worker"),
        Err(DynamicActorError::Unsupported)
    ));

    timeout(Duration::from_secs(1), handle.shutdown_and_wait())
        .await
        .expect("shutdown completed")
        .expect("supervisor shut down cleanly");
}
