use std::{future::pending, marker::PhantomData, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_otp::{
    ActorContext, ActorRef, ActorResult, DynamicActorOptions, GraphBuilder, RawActor, Runtime,
    SendError, SupervisedActors,
};
use tokio_supervisor::{ChildSpec, ControlError, ShutdownPolicy, Strategy, SupervisorBuilder};

fn build_runtime(graph: tokio_otp::Graph) -> Runtime {
    SupervisedActors::new(graph)
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

enum ForwardMsg {
    Target(ActorRef<String>),
    Forward(String),
}

#[derive(Clone)]
struct Forwarder;

impl RawActor for Forwarder {
    type Msg = ForwardMsg;

    async fn run(&self, mut ctx: ActorContext<ForwardMsg>) -> ActorResult {
        let mut target = None;
        while let Some(message) = ctx.recv().await {
            match message {
                ForwardMsg::Target(actor_ref) => target = Some(actor_ref),
                ForwardMsg::Forward(message) => {
                    target
                        .as_ref()
                        .expect("target distributed before forwarding")
                        .send(message)
                        .await?
                }
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn graphless_runtime_adds_removes_and_readds_actors() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let handle = Runtime::builder()
        .build()
        .expect("graphless runtime builds")
        .spawn();

    assert!(handle.snapshot().children.is_empty());
    let sink = handle
        .add_actor(
            "sink",
            Observe {
                observed: observed_tx.clone(),
            },
            DynamicActorOptions::default(),
        )
        .await
        .expect("sink added");
    sink.send("first".to_owned()).await.expect("message sent");
    assert_eq!(observed_rx.recv().await.as_deref(), Some("first"));

    handle.remove_child("sink").await.expect("sink removed");
    assert!(matches!(
        sink.send("after-remove".to_owned()).await,
        Err(SendError::ActorTerminated { actor_id }) if actor_id == "sink"
    ));

    let replacement = handle
        .add_actor(
            "sink",
            Observe {
                observed: observed_tx,
            },
            DynamicActorOptions::default(),
        )
        .await
        .expect("label can be reused");
    replacement
        .send("second".to_owned())
        .await
        .expect("replacement receives");
    assert_eq!(observed_rx.recv().await.as_deref(), Some("second"));

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn runtime_added_ref_is_distributed_to_static_actor_by_message() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let forwarder = builder.actor("forwarder", Forwarder);
    let handle = build_runtime(builder.build().expect("valid graph")).spawn();

    let sink = handle
        .add_actor(
            "sink",
            Observe {
                observed: observed_tx,
            },
            DynamicActorOptions::default(),
        )
        .await
        .expect("sink added");
    forwarder
        .send(ForwardMsg::Target(sink))
        .await
        .expect("typed ref distributed");
    forwarder
        .send(ForwardMsg::Forward("forwarded".to_owned()))
        .await
        .expect("message forwarded");

    assert_eq!(observed_rx.recv().await.as_deref(), Some("forwarded"));
    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[derive(Clone)]
struct ForwardTo {
    target: ActorRef<String>,
}

impl RawActor for ForwardTo {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            self.target.send(message).await?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn runtime_added_actor_can_receive_static_ref_at_creation() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let sink = builder.actor(
        "sink",
        Observe {
            observed: observed_tx,
        },
    );
    let handle = build_runtime(builder.build().expect("valid graph")).spawn();

    let dynamic = handle
        .add_actor(
            "dynamic",
            ForwardTo { target: sink },
            DynamicActorOptions::default(),
        )
        .await
        .expect("dynamic actor added");
    dynamic
        .send("forwarded".to_owned())
        .await
        .expect("dynamic actor receives");
    assert_eq!(observed_rx.recv().await.as_deref(), Some("forwarded"));

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[derive(Clone)]
struct PendingActor;

impl RawActor for PendingActor {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        pending::<()>().await;
        Ok(())
    }
}

#[tokio::test]
async fn timed_out_removal_terminates_the_typed_ref() {
    let handle = Runtime::builder().build().expect("runtime builds").spawn();
    let actor_ref = handle
        .add_actor(
            "dynamic",
            PendingActor,
            DynamicActorOptions {
                shutdown: ShutdownPolicy::cooperative_strict(Duration::from_millis(20)),
                ..DynamicActorOptions::default()
            },
        )
        .await
        .expect("actor added");

    assert!(matches!(
        handle.remove_child("dynamic").await,
        Err(ControlError::ShutdownTimedOut(actor_id)) if actor_id == "dynamic"
    ));
    assert!(matches!(
        actor_ref.send(()).await,
        Err(SendError::ActorTerminated { actor_id }) if actor_id == "dynamic"
    ));

    handle
        .add_actor(
            "dynamic",
            Drain::<()>::new(),
            DynamicActorOptions::default(),
        )
        .await
        .expect("label reusable after timed-out removal");
    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn runtime_new_supports_runtime_added_actors() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("seed", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");
    let handle = Runtime::new(supervisor).spawn();

    let actor_ref = handle
        .add_actor(
            "dynamic",
            Drain::<()>::new(),
            DynamicActorOptions::default(),
        )
        .await
        .expect("all runtimes support actor creation");
    assert_eq!(actor_ref.id(), "dynamic");

    timeout(Duration::from_secs(1), handle.shutdown_and_wait())
        .await
        .expect("shutdown completed")
        .expect("clean shutdown");
}
