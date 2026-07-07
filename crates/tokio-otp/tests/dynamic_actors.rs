use std::{collections::HashMap, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_actor::{ActorContext, ActorSpec, Envelope, SendError};
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

#[tokio::test]
async fn static_actor_can_send_to_runtime_added_actor() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let graph = tokio_actor::GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "frontend",
            |mut ctx: ActorContext| async move {
                while let Some(envelope) = ctx.recv().await {
                    ctx.send_dynamic_when_ready("dynamic", envelope).await?;
                }
                Ok(())
            },
        ))
        .ingress("requests", "frontend")
        .build()
        .expect("valid graph");

    let runtime = build_runtime(graph);
    let handle = runtime.spawn();
    let mut ingress = handle.ingress("requests").expect("ingress exists");

    assert!(handle.actor_ref("frontend").is_some());

    let mut dynamic_ref = handle
        .add_actor(
            ActorSpec::from_actor("dynamic", move |mut ctx: ActorContext| {
                let observed_tx = observed_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        observed_tx.send(envelope).expect("receiver alive");
                    }
                    Ok(())
                }
            }),
            DynamicActorOptions::default(),
        )
        .await
        .expect("dynamic actor added");

    dynamic_ref.wait_for_binding().await;
    ingress.wait_for_binding().await;
    ingress
        .send(Envelope::from_static(b"hello-dynamic"))
        .await
        .expect("message sent");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("dynamic actor observed the message")
        .expect("dynamic actor is still running");
    assert_eq!(observed.as_slice(), b"hello-dynamic");

    assert!(handle.actor_ref("dynamic").is_some());
    assert_eq!(
        handle
            .shutdown_and_wait()
            .await
            .expect("runtime shut down cleanly"),
        SupervisorExit::Shutdown
    );
}

#[tokio::test]
async fn runtime_added_actor_can_use_configured_peer_ids() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let graph = tokio_actor::GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "sink",
            move |mut ctx: ActorContext| {
                let observed_tx = observed_tx.clone();
                async move {
                    while let Some(envelope) = ctx.recv().await {
                        observed_tx.send(envelope).expect("receiver alive");
                    }
                    Ok(())
                }
            },
        ))
        .build()
        .expect("valid graph");

    let runtime = build_runtime(graph);
    let handle = runtime.spawn();

    let mut dynamic_ref = handle
        .add_actor(
            ActorSpec::from_actor("dynamic", |mut ctx: ActorContext| async move {
                while let Some(envelope) = ctx.recv().await {
                    ctx.send("sink", envelope).await?;
                }
                Ok(())
            }),
            DynamicActorOptions {
                peer_ids: vec!["sink".to_owned()],
                ..DynamicActorOptions::default()
            },
        )
        .await
        .expect("dynamic actor added");

    dynamic_ref.wait_for_binding().await;
    dynamic_ref
        .send(Envelope::from_static(b"forwarded"))
        .await
        .expect("message sent to dynamic actor");

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("sink observed the forwarded message")
        .expect("sink is still running");
    assert_eq!(observed.as_slice(), b"forwarded");

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime shut down cleanly");
}

#[tokio::test]
async fn remove_actor_deregisters_runtime_added_actor() {
    let graph = tokio_actor::GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "seed",
            |ctx: ActorContext| async move {
                ctx.shutdown_token().cancelled().await;
                Ok(())
            },
        ))
        .build()
        .expect("valid graph");

    let runtime = build_runtime(graph);
    let handle = runtime.spawn();

    let mut dynamic_ref = handle
        .add_actor(
            ActorSpec::from_actor("dynamic", |ctx: ActorContext| async move {
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }),
            DynamicActorOptions::default(),
        )
        .await
        .expect("dynamic actor added");

    dynamic_ref.wait_for_binding().await;
    handle
        .remove_actor("dynamic")
        .await
        .expect("dynamic actor removed");

    assert!(handle.actor_ref("dynamic").is_none());
    assert!(matches!(
        dynamic_ref.send(Envelope::from_static(b"gone")).await,
        Err(SendError::ActorNotRunning { actor_id }) if actor_id == "dynamic"
    ));

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime shut down cleanly");
}

#[tokio::test]
async fn timed_out_remove_actor_deregisters_runtime_added_actor() {
    let graph = tokio_actor::GraphBuilder::new()
        .actor(ActorSpec::from_actor(
            "seed",
            |ctx: ActorContext| async move {
                ctx.shutdown_token().cancelled().await;
                Ok(())
            },
        ))
        .build()
        .expect("valid graph");

    let runtime = build_runtime(graph);
    let handle = runtime.spawn();

    let mut dynamic_ref = handle
        .add_actor(
            ActorSpec::from_actor("dynamic", |_ctx: ActorContext| async move {
                std::future::pending::<()>().await;
                Ok(())
            }),
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

    assert!(handle.actor_ref("dynamic").is_none());
    assert!(
        handle
            .add_actor(
                ActorSpec::from_actor("dynamic", |ctx: ActorContext| async move {
                    ctx.shutdown_token().cancelled().await;
                    Ok(())
                }),
                DynamicActorOptions::default(),
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

    let runtime = Runtime::new(supervisor, HashMap::new());
    let handle = runtime.spawn();

    let err = handle
        .add_actor(
            ActorSpec::from_actor("dynamic", |ctx: ActorContext| async move {
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }),
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
