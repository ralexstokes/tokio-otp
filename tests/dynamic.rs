use std::sync::Arc;

use tokio::{
    sync::{Notify, mpsc},
    time::timeout,
};
use tokio_supervisor::{
    ChildSpec, ControlError, Restart, ShutdownMode, ShutdownPolicy, SupervisorBuilder,
    SupervisorEvent, SupervisorExit,
};

mod common;

#[tokio::test]
async fn add_child_starts_it_immediately() {
    let (dynamic_tx, mut dynamic_rx) = mpsc::unbounded_channel();

    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("seed", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    handle
        .add_child(ChildSpec::new("dynamic", move |ctx| {
            let dynamic_tx = dynamic_tx.clone();
            async move {
                dynamic_tx
                    .send(ctx.generation)
                    .expect("test receiver dropped");
                ctx.token.cancelled().await;
                Ok(())
            }
        }))
        .await
        .expect("dynamic child should be accepted");

    assert_eq!(common::recv_event(&mut dynamic_rx).await, 0);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn remove_child_stops_it_without_restarting() {
    let (starts_tx, mut starts_rx) = mpsc::unbounded_channel();
    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("removable", move |ctx| {
                let starts_tx = starts_tx.clone();
                async move {
                    starts_tx
                        .send(ctx.generation)
                        .expect("test receiver dropped");
                    ctx.token.cancelled().await;
                    Err(common::test_error("do not restart on remove"))
                }
            })
            .restart(Restart::Transient),
        )
        .child(ChildSpec::new("keeper", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    assert_eq!(common::recv_event(&mut starts_rx).await, 0);

    handle
        .remove_child("removable")
        .await
        .expect("child removal should succeed");

    let mut saw_remove_requested = false;
    let mut saw_removed = false;
    while !saw_remove_requested || !saw_removed {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::ChildRemoveRequested { id } if id == "removable" => {
                saw_remove_requested = true;
            }
            SupervisorEvent::ChildRemoved { id } if id == "removable" => {
                saw_removed = true;
            }
            _ => {}
        }
    }

    common::assert_no_event(&mut starts_rx).await;

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn duplicate_add_and_unknown_remove_are_rejected() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("seed", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    let duplicate = handle
        .add_child(ChildSpec::new("seed", |_ctx| async move { Ok(()) }))
        .await
        .expect_err("duplicate id should be rejected");
    assert_eq!(duplicate, ControlError::DuplicateChildId("seed".to_owned()));

    let missing = handle
        .remove_child("missing")
        .await
        .expect_err("unknown child id should be rejected");
    assert_eq!(missing, ControlError::UnknownChildId("missing".to_owned()));

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn removing_the_last_active_child_is_rejected() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("only", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    let err = handle
        .remove_child("only")
        .await
        .expect_err("last child removal should be rejected");
    assert_eq!(err, ControlError::LastChildRemovalUnsupported);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn concurrent_removal_requests_are_serialized() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());

    let release_for_child = release.clone();
    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("removable", move |ctx| {
                let started_tx = started_tx.clone();
                let cancelled_tx = cancelled_tx.clone();
                let release = release_for_child.clone();
                async move {
                    started_tx.send(()).expect("test receiver dropped");
                    ctx.token.cancelled().await;
                    cancelled_tx.send(()).expect("test receiver dropped");
                    release.notified().await;
                    Ok(())
                }
            })
            .restart(Restart::Transient),
        )
        .child(ChildSpec::new("keeper", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    common::recv_event(&mut started_rx).await;

    let remove_handle = handle.clone();
    let remove_task = tokio::spawn(async move { remove_handle.remove_child("removable").await });

    common::recv_event(&mut cancelled_rx).await;

    let second_remove_handle = handle.clone();
    let mut second_remove_task =
        tokio::spawn(async move { second_remove_handle.remove_child("removable").await });

    timeout(common::QUIET_TIMEOUT, &mut second_remove_task)
        .await
        .expect_err("second removal should remain queued while the first is pending");

    release.notify_one();
    remove_task
        .await
        .expect("remove task should join")
        .expect("first removal should succeed");

    let err = second_remove_task
        .await
        .expect("second remove task should join")
        .expect_err("second removal should observe the completed first removal");
    assert_eq!(err, ControlError::UnknownChildId("removable".to_owned()));

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn removal_returns_supervisor_stopping_when_shutdown_intervenes() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let fast_shutdown = ShutdownPolicy {
        grace: common::SHORT_GRACE,
        mode: ShutdownMode::CooperativeThenAbort,
    };

    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("removable", move |ctx| {
                let started_tx = started_tx.clone();
                let cancelled_tx = cancelled_tx.clone();
                async move {
                    started_tx.send(()).expect("test receiver dropped");
                    ctx.token.cancelled().await;
                    cancelled_tx.send(()).expect("test receiver dropped");
                    std::future::pending::<()>().await;
                    Ok(())
                }
            })
            .restart(Restart::Transient)
            .shutdown(fast_shutdown),
        )
        .child(
            ChildSpec::new("keeper", |ctx| async move {
                ctx.token.cancelled().await;
                Ok(())
            })
            .shutdown(fast_shutdown),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    common::recv_event(&mut started_rx).await;

    let remove_handle = handle.clone();
    let remove_task = tokio::spawn(async move { remove_handle.remove_child("removable").await });

    common::recv_event(&mut cancelled_rx).await;
    handle.shutdown();

    let err = remove_task
        .await
        .expect("remove task should join")
        .expect_err("removal should abort once supervisor shutdown begins");
    assert_eq!(err, ControlError::SupervisorStopping);

    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn control_plane_is_unavailable_after_supervisor_exit() {
    let handle = SupervisorBuilder::new()
        .child(ChildSpec::new("done", |_ctx| async move { Ok(()) }).restart(Restart::Temporary))
        .build()
        .expect("valid supervisor")
        .spawn();

    let exit = handle
        .wait()
        .await
        .expect("supervisor should complete cleanly");
    assert!(matches!(exit, SupervisorExit::Completed));

    let add_err = handle
        .add_child(ChildSpec::new("late", |_ctx| async move { Ok(()) }))
        .await
        .expect_err("control plane should be closed after exit");
    assert_eq!(add_err, ControlError::Unavailable);

    let remove_err = handle
        .remove_child("done")
        .await
        .expect_err("control plane should be closed after exit");
    assert_eq!(remove_err, ControlError::Unavailable);
}

#[tokio::test]
async fn try_add_child_returns_busy_when_control_queue_is_full() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());

    let release_for_child = release.clone();
    let supervisor = SupervisorBuilder::new()
        .control_channel_capacity(1)
        .child(
            ChildSpec::new("removable", move |ctx| {
                let started_tx = started_tx.clone();
                let cancelled_tx = cancelled_tx.clone();
                let release = release_for_child.clone();
                async move {
                    started_tx.send(()).expect("test receiver dropped");
                    ctx.token.cancelled().await;
                    cancelled_tx.send(()).expect("test receiver dropped");
                    release.notified().await;
                    Ok(())
                }
            })
            .restart(Restart::Transient),
        )
        .child(ChildSpec::new("keeper", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    common::recv_event(&mut started_rx).await;

    let remove_handle = handle.clone();
    let remove_task = tokio::spawn(async move { remove_handle.remove_child("removable").await });

    common::recv_event(&mut cancelled_rx).await;

    let queued_handle = handle.clone();
    let mut queued_add = tokio::spawn(async move {
        queued_handle
            .add_child(ChildSpec::new("queued", |ctx| async move {
                ctx.token.cancelled().await;
                Ok(())
            }))
            .await
    });

    timeout(common::QUIET_TIMEOUT, &mut queued_add)
        .await
        .expect_err("queued add should remain pending while removal is blocked");

    let err = handle
        .try_add_child(ChildSpec::new("busy", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .await
        .expect_err("queue-full try_add_child should fail fast");
    assert_eq!(err, ControlError::Busy);

    release.notify_one();
    remove_task
        .await
        .expect("remove task should join")
        .expect("child removal should succeed");
    queued_add
        .await
        .expect("queued add task should join")
        .expect("queued add should succeed once capacity frees");

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn remove_child_completes_promptly_during_restart_backoff() {
    let (starts_tx, mut starts_rx) = mpsc::unbounded_channel();

    let handle = SupervisorBuilder::new()
        .restart_intensity(tokio_supervisor::RestartIntensity {
            max_restarts: 4,
            within: std::time::Duration::from_secs(1),
            backoff: tokio_supervisor::BackoffPolicy::Fixed(std::time::Duration::from_secs(1)),
        })
        .child(
            ChildSpec::new("removable", move |ctx| {
                let starts_tx = starts_tx.clone();
                async move {
                    starts_tx
                        .send(ctx.generation)
                        .expect("test receiver dropped");
                    Err(common::test_error("restart me later"))
                }
            })
            .restart(Restart::Transient),
        )
        .child(ChildSpec::new("keeper", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor")
        .spawn();
    let mut events = handle.subscribe();

    assert_eq!(common::recv_event(&mut starts_rx).await, 0);

    loop {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::ChildRestartScheduled { id, delay, .. } if id == "removable" => {
                assert!(
                    delay >= std::time::Duration::from_secs(1),
                    "test requires a long restart backoff"
                );
                break;
            }
            _ => {}
        }
    }

    timeout(common::QUIET_TIMEOUT, handle.remove_child("removable"))
        .await
        .expect("remove_child should not wait for the restart backoff")
        .expect("child removal should succeed during backoff");

    let mut saw_removed = false;
    while !saw_removed {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::ChildRemoved { id } if id == "removable" => {
                saw_removed = true;
            }
            SupervisorEvent::ChildStarted { id, generation }
                if id == "removable" && generation > 0 =>
            {
                panic!("removed child restarted while removal was pending");
            }
            _ => {}
        }
    }

    common::assert_no_event(&mut starts_rx).await;

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}
