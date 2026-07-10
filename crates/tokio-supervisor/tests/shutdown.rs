use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_supervisor::{
    BackoffPolicy, ChildSpec, ControlError, RestartIntensity, RestartPolicy, ShutdownMode,
    ShutdownPolicy, SupervisorBuilder, SupervisorError, SupervisorEvent,
};

mod common;

#[tokio::test]
async fn external_shutdown_stops_all_children() {
    let exits = Arc::new(AtomicUsize::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let make_child = |id: &'static str, exits: Arc<AtomicUsize>| {
        let started_tx = started_tx.clone();
        ChildSpec::new(id, move |ctx| {
            let exits = exits.clone();
            let started_tx = started_tx.clone();
            async move {
                started_tx.send(()).expect("test receiver dropped");
                ctx.shutdown_token().cancelled().await;
                exits.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
    };

    let supervisor = SupervisorBuilder::new()
        .child(make_child("worker-a", exits.clone()))
        .child(make_child("worker-b", exits.clone()))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    common::recv_event(&mut started_rx).await;
    common::recv_event(&mut started_rx).await;
    handle.shutdown();

    handle.wait().await.expect("shutdown should succeed");
    assert_eq!(exits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn shutdown_is_idempotent_across_handle_clones() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let clone = handle.clone();

    handle.shutdown();
    clone.shutdown();
    handle.shutdown();

    handle.wait().await.expect("first waiter should resolve");
    clone.wait().await.expect("second waiter should resolve");
}

#[tokio::test]
async fn dropping_last_handle_clone_requests_graceful_shutdown() {
    let (lifecycle_tx, mut lifecycle_rx) = mpsc::unbounded_channel();

    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", move |ctx| {
            let lifecycle_tx = lifecycle_tx.clone();
            async move {
                lifecycle_tx.send("started").expect("test receiver dropped");
                ctx.shutdown_token().cancelled().await;
                lifecycle_tx
                    .send("cancelled")
                    .expect("test receiver dropped");
                Ok(())
            }
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();
    let last_handle = handle.clone();

    assert_eq!(common::recv_event(&mut lifecycle_rx).await, "started");

    drop(handle);
    common::assert_no_event(&mut lifecycle_rx).await;

    drop(last_handle);
    assert_eq!(common::recv_event(&mut lifecycle_rx).await, "cancelled");

    loop {
        if common::recv_supervisor_event(&mut events).await == SupervisorEvent::SupervisorStopped {
            break;
        }
    }
}

#[tokio::test]
async fn dropping_last_handle_stops_a_supervisor_idling_at_zero_children() {
    let supervisor = SupervisorBuilder::new()
        .build()
        .expect("empty supervisor builds");

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    drop(handle);

    loop {
        if common::recv_supervisor_event(&mut events).await == SupervisorEvent::SupervisorStopped {
            break;
        }
    }
}

#[tokio::test]
async fn cooperative_child_observes_cancellation_before_shutdown_finishes() {
    let saw_cancel = Arc::new(AtomicBool::new(false));

    let saw_cancel_for_child = saw_cancel.clone();
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", move |ctx| {
            let saw_cancel = saw_cancel_for_child.clone();
            async move {
                ctx.shutdown_token().cancelled().await;
                saw_cancel.store(true, Ordering::SeqCst);
                Ok(())
            }
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    handle.shutdown();

    handle.wait().await.expect("shutdown should succeed");
    assert!(saw_cancel.load(Ordering::SeqCst));
}

#[tokio::test]
async fn stubborn_child_is_aborted_in_cooperative_then_abort_mode() {
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let live_flag = common::LiveFlag::new();

    let saw_cancel_for_child = saw_cancel.clone();
    let live_flag_for_child = live_flag.clone();
    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("stubborn", move |_ctx| {
                let saw_cancel = saw_cancel_for_child.clone();
                let live_flag = live_flag_for_child.clone();
                async move {
                    let _guard = live_flag.guard();
                    loop {
                        sleep(Duration::from_millis(10)).await;
                        let _ = saw_cancel.load(Ordering::SeqCst);
                    }
                }
            })
            .shutdown(ShutdownPolicy {
                grace: common::SHORT_GRACE,
                mode: ShutdownMode::CooperativeThenAbort,
            }),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    handle.shutdown();

    handle.wait().await.expect("shutdown should succeed");
    assert!(!saw_cancel.load(Ordering::SeqCst));
    assert!(
        !live_flag.is_live(),
        "task should be dropped before wait resolves"
    );
}

#[tokio::test]
async fn cooperative_shutdown_times_out_with_stuck_child_name() {
    let live_flag = common::LiveFlag::new();

    let live_flag_for_child = live_flag.clone();
    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("stubborn", move |_ctx| {
                let live_flag = live_flag_for_child.clone();
                async move {
                    let _guard = live_flag.guard();
                    loop {
                        sleep(Duration::from_millis(10)).await;
                    }
                }
            })
            .shutdown(ShutdownPolicy {
                grace: common::SHORT_GRACE,
                mode: ShutdownMode::CooperativeStrict,
            }),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    handle.shutdown();

    let err = handle
        .wait()
        .await
        .expect_err("pure cooperative shutdown should time out");
    assert_eq!(
        err,
        SupervisorError::ShutdownTimedOut("stubborn".to_owned())
    );
    assert!(
        !live_flag.is_live(),
        "timed-out cooperative child should be aborted before wait resolves"
    );
}

#[tokio::test]
async fn mixed_shutdown_only_reports_pure_cooperative_children() {
    let cooperative_live_flag = common::LiveFlag::new();
    let aborting_live_flag = common::LiveFlag::new();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let started_tx_for_aborting = started_tx.clone();
    let aborting_live_flag_for_child = aborting_live_flag.clone();
    let started_tx_for_cooperative = started_tx.clone();
    let cooperative_live_flag_for_child = cooperative_live_flag.clone();
    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("cooperative-then-abort", move |_ctx| {
                let started_tx = started_tx_for_aborting.clone();
                let live_flag = aborting_live_flag_for_child.clone();
                async move {
                    let _guard = live_flag.guard();
                    started_tx.send(()).expect("test receiver dropped");
                    loop {
                        sleep(Duration::from_millis(10)).await;
                    }
                }
            })
            .shutdown(ShutdownPolicy {
                grace: common::SHORT_GRACE,
                mode: ShutdownMode::CooperativeThenAbort,
            }),
        )
        .child(
            ChildSpec::new("cooperative", move |_ctx| {
                let started_tx = started_tx_for_cooperative.clone();
                let live_flag = cooperative_live_flag_for_child.clone();
                async move {
                    let _guard = live_flag.guard();
                    started_tx.send(()).expect("test receiver dropped");
                    loop {
                        sleep(Duration::from_millis(10)).await;
                    }
                }
            })
            .shutdown(ShutdownPolicy {
                grace: common::SHORT_GRACE,
                mode: ShutdownMode::CooperativeStrict,
            }),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    common::recv_n(&mut started_rx, 2).await;

    handle.shutdown();

    let err = handle
        .wait()
        .await
        .expect_err("mixed shutdown should still report cooperative timeouts");
    assert_eq!(
        err,
        SupervisorError::ShutdownTimedOut("cooperative".to_owned())
    );
    assert!(
        !cooperative_live_flag.is_live(),
        "timed-out cooperative child should be aborted before wait resolves"
    );
    assert!(
        !aborting_live_flag.is_live(),
        "cooperative-then-abort child should also be aborted before wait resolves"
    );
}

#[tokio::test]
async fn cooperative_remove_child_times_out_with_stuck_child_name() {
    let live_flag = common::LiveFlag::new();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let live_flag_for_child = live_flag.clone();
    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("stubborn", move |_ctx| {
                let started_tx = started_tx.clone();
                let live_flag = live_flag_for_child.clone();
                async move {
                    let _guard = live_flag.guard();
                    started_tx.send(()).expect("test receiver dropped");
                    loop {
                        sleep(Duration::from_millis(10)).await;
                    }
                }
            })
            .shutdown(ShutdownPolicy {
                grace: common::SHORT_GRACE,
                mode: ShutdownMode::CooperativeStrict,
            }),
        )
        .child(ChildSpec::new("keeper", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    common::recv_event(&mut started_rx).await;

    let err = handle
        .remove_child("stubborn")
        .await
        .expect_err("pure cooperative child removal should time out");
    assert_eq!(err, ControlError::ShutdownTimedOut("stubborn".to_owned()));
    assert!(
        !live_flag.is_live(),
        "timed-out cooperative removal should abort the child before returning"
    );

    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn wait_only_resolves_after_child_lifetimes_end() {
    let live_flag = common::LiveFlag::new();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let live_flag_for_child = live_flag.clone();
    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("stubborn", move |_ctx| {
                let started_tx = started_tx.clone();
                let live_flag = live_flag_for_child.clone();
                async move {
                    let _guard = live_flag.guard();
                    started_tx.send(()).expect("test receiver dropped");
                    loop {
                        sleep(Duration::from_millis(10)).await;
                    }
                }
            })
            .shutdown(ShutdownPolicy {
                grace: common::SHORT_GRACE,
                mode: ShutdownMode::Abort,
            }),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    common::recv_event(&mut started_rx).await;
    assert!(live_flag.is_live());

    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
    assert!(
        !live_flag.is_live(),
        "child must be dropped before wait completes"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_preempts_zero_delay_restart() {
    let supervisor = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 8,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
        .child(
            ChildSpec::new("flaky", |_ctx| async move {
                Err(common::test_error("restart immediately"))
            })
            .restart(RestartPolicy::OnFailure),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    loop {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::ChildRestartScheduled { delay, .. } => {
                assert!(delay.is_zero(), "test requires zero-delay restart");
                handle.shutdown();
                break;
            }
            SupervisorEvent::RestartIntensityExceeded => {
                panic!("shutdown lost to zero-delay restart");
            }
            _ => {}
        }
    }

    loop {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::SupervisorStopping => break,
            SupervisorEvent::ChildRestarted { .. } => {
                panic!("child restarted after shutdown was requested");
            }
            SupervisorEvent::RestartIntensityExceeded => {
                panic!("shutdown lost to zero-delay restart");
            }
            _ => {}
        }
    }

    handle.wait().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn shutdown_preempts_delayed_restart_in_cooperative_mode() {
    let saw_cancel = Arc::new(AtomicBool::new(false));

    let saw_cancel_for_keeper = saw_cancel.clone();
    let supervisor = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 8,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::Fixed(Duration::from_millis(200)),
        })
        .child(
            ChildSpec::new("flaky", |_ctx| async move {
                Err(common::test_error("restart later"))
            })
            .restart(RestartPolicy::OnFailure),
        )
        .child(
            ChildSpec::new("keeper", move |ctx| {
                let saw_cancel = saw_cancel_for_keeper.clone();
                async move {
                    ctx.shutdown_token().cancelled().await;
                    saw_cancel.store(true, Ordering::SeqCst);
                    Ok(())
                }
            })
            .shutdown(ShutdownPolicy {
                grace: common::SHORT_GRACE,
                mode: ShutdownMode::CooperativeStrict,
            }),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    loop {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::ChildRestartScheduled { id, delay, .. } if id == "flaky" => {
                assert!(
                    delay >= Duration::from_millis(200),
                    "test requires a non-zero delayed restart"
                );
                handle.shutdown();
                break;
            }
            SupervisorEvent::RestartIntensityExceeded => {
                panic!("shutdown lost to delayed restart");
            }
            _ => {}
        }
    }

    loop {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::SupervisorStopping => break,
            SupervisorEvent::ChildRestarted { id, .. } if id == "flaky" => {
                panic!("child restarted after shutdown was requested");
            }
            SupervisorEvent::RestartIntensityExceeded => {
                panic!("shutdown lost to delayed restart");
            }
            _ => {}
        }
    }

    handle.wait().await.expect("shutdown should succeed");
    assert!(
        saw_cancel.load(Ordering::SeqCst),
        "cooperative child should observe shutdown cancellation"
    );
}
