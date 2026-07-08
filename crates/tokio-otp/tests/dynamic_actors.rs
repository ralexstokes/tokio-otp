use std::{future::pending, marker::PhantomData, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{Actor, ActorContext, ActorResult, GraphBuilder, LookupError, SendError};
use tokio_otp::{DynamicActorError, DynamicActorOptions, Runtime, SupervisedActors};
use tokio_supervisor::{
    ChildSpec, ControlError, ShutdownPolicy, Strategy, SupervisorBuilder, SupervisorExit,
};

fn build_runtime(graph: tokio_actor::Graph) -> Runtime {
    SupervisedActors::new(graph)
        .expect("graph decomposes")
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("runtime builds")
}

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
struct ForwardToDynamic;

impl Actor for ForwardToDynamic {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            let mut dynamic = ctx
                .registry()
                .expect("registry installed")
                .actor_ref::<String>("dynamic")?;
            dynamic.send_when_ready(message).await?;
        }
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

#[tokio::test]
async fn static_actor_can_send_to_runtime_added_actor() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let mut frontend = builder.actor("frontend", ForwardToDynamic);
    let graph = builder.build().expect("valid graph");

    let runtime = build_runtime(graph);
    let handle = runtime.spawn();

    assert!(handle.actor_ref::<String>("frontend").is_ok());

    let mut dynamic_ref = handle
        .add_actor(
            "dynamic",
            Observe {
                observed: observed_tx,
            },
            DynamicActorOptions::default(),
        )
        .await
        .expect("dynamic actor added");

    dynamic_ref.wait_for_binding().await;
    frontend.wait_for_binding().await;
    frontend
        .send("hello-dynamic".to_owned())
        .await
        .expect("message sent");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("dynamic actor observed the message")
        .expect("dynamic actor is still running");
    assert_eq!(observed, "hello-dynamic");

    assert!(handle.actor_ref::<String>("dynamic").is_ok());
    assert_eq!(
        handle
            .shutdown_and_wait()
            .await
            .expect("runtime shut down cleanly"),
        SupervisorExit::Shutdown
    );
}

#[derive(Clone)]
struct ForwardToSink;

impl Actor for ForwardToSink {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            let sink = ctx
                .registry()
                .expect("registry installed")
                .actor_ref::<String>("sink")?;
            sink.send(message).await?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn runtime_added_actor_can_use_registry_to_reach_static_actor() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    builder.actor(
        "sink",
        Observe {
            observed: observed_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let runtime = build_runtime(graph);
    let handle = runtime.spawn();

    let mut dynamic_ref = handle
        .add_actor("dynamic", ForwardToSink, DynamicActorOptions::default())
        .await
        .expect("dynamic actor added");

    dynamic_ref.wait_for_binding().await;
    dynamic_ref
        .send("forwarded".to_owned())
        .await
        .expect("message sent to dynamic actor");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("sink observed the forwarded message")
        .expect("sink is still running");
    assert_eq!(observed, "forwarded");

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime shut down cleanly");
}

#[tokio::test]
async fn remove_actor_deregisters_runtime_added_actor() {
    let mut builder = GraphBuilder::new();
    builder.actor("seed", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let runtime = build_runtime(graph);
    let handle = runtime.spawn();

    let mut dynamic_ref = handle
        .add_actor(
            "dynamic",
            Drain::<()>::new(),
            DynamicActorOptions::default(),
        )
        .await
        .expect("dynamic actor added");

    dynamic_ref.wait_for_binding().await;
    handle
        .remove_actor("dynamic")
        .await
        .expect("dynamic actor removed");

    assert!(matches!(
        handle.actor_ref::<()>("dynamic"),
        Err(LookupError::UnknownActor { .. })
    ));
    assert!(matches!(
        dynamic_ref.send(()).await,
        Err(SendError::ActorNotRunning { actor_id }) if actor_id == "dynamic"
    ));

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime shut down cleanly");
}

#[derive(Clone)]
struct PendingActor;

impl Actor for PendingActor {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        pending::<()>().await;
        Ok(())
    }
}

#[tokio::test]
async fn timed_out_remove_actor_deregisters_runtime_added_actor() {
    let mut builder = GraphBuilder::new();
    builder.actor("seed", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let runtime = build_runtime(graph);
    let handle = runtime.spawn();

    let mut dynamic_ref = handle
        .add_actor(
            "dynamic",
            PendingActor,
            DynamicActorOptions {
                shutdown: ShutdownPolicy::cooperative(Duration::from_millis(20)),
                ..DynamicActorOptions::default()
            },
        )
        .await
        .expect("dynamic actor added");

    dynamic_ref.wait_for_binding().await;

    let err = handle
        .remove_actor("dynamic")
        .await
        .expect_err("cooperative removal should report timeout");
    assert!(matches!(
        err,
        DynamicActorError::Control(ControlError::ShutdownTimedOut(ref actor_id))
            if actor_id == "dynamic"
    ));

    assert!(matches!(
        handle.actor_ref::<()>("dynamic"),
        Err(LookupError::UnknownActor { .. })
    ));
    assert!(
        handle
            .add_actor(
                "dynamic",
                Drain::<()>::new(),
                DynamicActorOptions::default()
            )
            .await
            .is_ok(),
        "stale registry entry should not block re-adding the actor id"
    );

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime shut down cleanly");
}

#[tokio::test]
async fn manual_runtime_reports_dynamic_support_as_unavailable() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("seed", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let runtime = Runtime::new(supervisor);
    let handle = runtime.spawn();

    let err = handle
        .add_actor(
            "dynamic",
            Drain::<()>::new(),
            DynamicActorOptions::default(),
        )
        .await
        .expect_err("manual runtime should not support dynamic actors");
    assert!(matches!(err, DynamicActorError::Unsupported));

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime shut down cleanly");
}
