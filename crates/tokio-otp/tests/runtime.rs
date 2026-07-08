use std::{future::IntoFuture, io, marker::PhantomData, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{
    Actor, ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder, LookupError, SendError,
};
use tokio_otp::{BuildError, Runtime, SupervisedActors};
use tokio_supervisor::{
    Restart, RestartIntensity, Strategy, SupervisorBuilder, SupervisorExit, SupervisorStateView,
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

impl<M: Send + 'static> Actor for Drain<M> {
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

impl Actor for Observe {
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

impl Actor for ObserveOnce {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let message = ctx.recv().await.expect("message received before shutdown");
        self.observed.send(message).expect("receiver alive");
        Ok(())
    }
}

fn build_runtime<A>(actor: A) -> (Runtime, ActorRef<A::Msg>)
where
    A: Actor,
{
    let mut builder = GraphBuilder::new();
    let actor_ref = builder.actor("worker", actor);
    let graph = builder.build().expect("valid graph");

    let runtime = SupervisedActors::new(graph)
        .expect("graph decomposes")
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

    assert_eq!(
        handle
            .shutdown_and_wait()
            .await
            .expect("supervisor shut down cleanly"),
        SupervisorExit::Shutdown
    );
}

#[tokio::test]
async fn runtime_into_supervisor_run_accepts_ref_cloned_before_startup() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, worker_ref) = build_runtime(ObserveOnce {
        observed: observed_tx,
    });

    let sender = tokio::spawn(async move {
        worker_ref
            .send("run-path".to_owned())
            .await
            .expect("message sent through cloned ref");
    });

    assert_eq!(
        runtime
            .into_supervisor()
            .run()
            .await
            .expect("runtime exits cleanly"),
        SupervisorExit::Completed
    );
    sender.await.expect("sender task joined");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker observed the message")
        .expect("worker is still running");
    assert_eq!(observed, "run-path");
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
        .wait_until_running()
        .await
        .expect("runtime reported running");
    let _events = control.subscribe();
    assert_eq!(control.snapshot().children.len(), 1);

    worker_ref
        .send("spawn-wait-path".to_owned())
        .await
        .expect("message sent through cloned ref");

    assert_eq!(
        handle.wait().await.expect("runtime exits cleanly"),
        SupervisorExit::Completed
    );

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker observed the message")
        .expect("worker is still running");
    assert_eq!(observed, "spawn-wait-path");
}

#[tokio::test]
async fn runtime_handle_reports_unknown_actor_lookup() {
    let (runtime, _worker_ref) = build_runtime(Drain::<()>::new());

    let handle = runtime.spawn();
    assert!(matches!(
        handle.actor_ref::<()>("missing"),
        Err(LookupError::UnknownActor { .. })
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

    assert_eq!(
        handle
            .shutdown_and_wait()
            .await
            .expect("supervisor shut down cleanly"),
        SupervisorExit::Shutdown
    );
}

#[tokio::test]
async fn wait_until_running_resolves_after_spawn_with_multiple_children() {
    let mut builder = GraphBuilder::new();
    builder.actor("one", Drain::<()>::new());
    builder.actor("two", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let runtime = SupervisedActors::new(graph)
        .expect("graph decomposes")
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("runtime builds");
    let handle = runtime.spawn();

    timeout(Duration::from_secs(1), handle.wait_until_running())
        .await
        .expect("runtime reported running")
        .expect("runtime running result");
    assert_eq!(handle.snapshot().children.len(), 2);

    assert_eq!(
        handle
            .shutdown_and_wait()
            .await
            .expect("runtime shut down cleanly"),
        SupervisorExit::Shutdown
    );
}

#[derive(Clone)]
struct FailOnMessage;

impl Actor for FailOnMessage {
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

    assert_eq!(
        handle
            .shutdown_and_wait()
            .await
            .expect("runtime shut down cleanly"),
        SupervisorExit::Shutdown
    );
}

#[derive(Clone)]
struct AlwaysFails;

impl Actor for AlwaysFails {
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
        Err(BuildError::MissingGraph)
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
        Err(LookupError::UnknownActor { .. })
    ));

    assert_eq!(
        timeout(Duration::from_secs(1), handle.shutdown_and_wait())
            .await
            .expect("shutdown completed")
            .expect("supervisor shut down cleanly"),
        SupervisorExit::Shutdown
    );
}
