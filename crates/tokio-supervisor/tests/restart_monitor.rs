use std::{
    future::IntoFuture,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, watch},
    time::timeout,
};
use tokio_supervisor::{
    BackoffPolicy, ChildMembershipView, ChildSnapshot, ChildSpec, ChildStateView, RestartIntensity,
    RestartMonitorError, RestartPolicy, ShutdownMode, ShutdownPolicy, SupervisorBuilder,
    SupervisorError, SupervisorSnapshot,
};

mod common;

#[tokio::test]
async fn monitor_restart_resolves_after_child_returns_running() {
    let trigger_failure = Arc::new(Notify::new());
    let child = fail_on_generations("worker", trigger_failure.clone(), 1);
    let handle = SupervisorBuilder::new()
        .child(child)
        .build()
        .expect("valid supervisor")
        .spawn();

    let restart = handle
        .monitor_restart("worker")
        .expect("worker child should be known");
    assert_eq!(restart.id(), "worker");
    assert_eq!(restart.baseline_generation(), 0);

    trigger_failure.notify_one();

    let generation = timeout(common::EVENT_TIMEOUT, restart.into_future())
        .await
        .expect("restart monitor should resolve")
        .expect("restart monitor should succeed");
    assert_eq!(generation, 1);

    shutdown(handle).await;
}

#[tokio::test]
async fn monitor_created_after_completed_restart_waits_for_next_restart() {
    let trigger_failure = Arc::new(Notify::new());
    let child = fail_on_generations("worker", trigger_failure.clone(), 2);
    let handle = SupervisorBuilder::new()
        .child(child)
        .build()
        .expect("valid supervisor")
        .spawn();

    let mut snapshots = handle.subscribe_snapshots();
    trigger_failure.notify_one();
    wait_for_child_running(&mut snapshots, "worker", 1).await;

    let restart = handle
        .monitor_restart("worker")
        .expect("worker child should be known")
        .into_future();
    tokio::pin!(restart);

    timeout(common::QUIET_TIMEOUT, restart.as_mut())
        .await
        .expect_err("monitor should wait for the next restart");

    trigger_failure.notify_one();

    let generation = timeout(common::EVENT_TIMEOUT, restart.as_mut())
        .await
        .expect("restart monitor should resolve")
        .expect("restart monitor should succeed");
    assert_eq!(generation, 2);

    shutdown(handle).await;
}

#[tokio::test]
async fn monitor_restart_allows_coalesced_generations() {
    let trigger_failure = Arc::new(Notify::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_child = attempts.clone();
    let trigger_for_child = trigger_failure.clone();

    let child = ChildSpec::new("worker", move |ctx| {
        let attempts = attempts_for_child.clone();
        let trigger_failure = trigger_for_child.clone();
        async move {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                trigger_failure.notified().await;
                return Err(common::test_error("first crash"));
            }
            if attempt < 3 {
                return Err(common::test_error("coalesced crash"));
            }

            ctx.shutdown_token().cancelled().await;
            Ok(())
        }
    })
    .restart(RestartPolicy::OnFailure)
    .restart_intensity(RestartIntensity {
        max_restarts: 5,
        within: Duration::from_secs(1),
        backoff: BackoffPolicy::None,
    });

    let handle = SupervisorBuilder::new()
        .child(child)
        .build()
        .expect("valid supervisor")
        .spawn();

    let restart = handle
        .monitor_restart("worker")
        .expect("worker child should be known");
    let baseline = restart.baseline_generation();

    trigger_failure.notify_one();

    let generation = timeout(common::EVENT_TIMEOUT, restart.into_future())
        .await
        .expect("restart monitor should resolve")
        .expect("restart monitor should succeed");
    assert!(generation > baseline);

    let mut snapshots = handle.subscribe_snapshots();
    wait_for_child_running(&mut snapshots, "worker", 3).await;

    shutdown(handle).await;
}

#[tokio::test]
async fn monitor_restart_errors_when_child_is_removed() {
    let handle = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor")
        .spawn();

    let restart = handle
        .monitor_restart("worker")
        .expect("worker child should be known");
    handle
        .remove_child("worker")
        .await
        .expect("child removal should succeed");

    let result = timeout(common::EVENT_TIMEOUT, restart.into_future())
        .await
        .expect("restart monitor should resolve");
    assert_eq!(
        result,
        Err(RestartMonitorError::ChildRemoved("worker".to_owned()))
    );

    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn monitor_restart_errors_when_restart_intensity_is_exhausted() {
    let trigger_failure = Arc::new(Notify::new());
    let trigger_for_child = trigger_failure.clone();
    let child = ChildSpec::new("worker", move |_ctx| {
        let trigger_failure = trigger_for_child.clone();
        async move {
            trigger_failure.notified().await;
            Err(common::test_error("boom"))
        }
    })
    .restart(RestartPolicy::OnFailure);

    let handle = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity::new(0, Duration::from_secs(1)))
        .child(child)
        .build()
        .expect("valid supervisor")
        .spawn();

    let restart = handle
        .monitor_restart("worker")
        .expect("worker child should be known");
    trigger_failure.notify_one();

    let result = timeout(common::EVENT_TIMEOUT, restart.into_future())
        .await
        .expect("restart monitor should resolve");
    assert_eq!(result, Err(RestartMonitorError::SupervisorStopped));

    let error = handle
        .wait()
        .await
        .expect_err("supervisor should stop after intensity exhaustion");
    assert_eq!(error, SupervisorError::RestartIntensityExceeded);
}

#[tokio::test]
async fn monitor_restart_errors_synchronously_during_removal_window() {
    let handle = SupervisorBuilder::new()
        .child(
            ChildSpec::new("worker", |_ctx| async move {
                std::future::pending::<()>().await;
                Ok(())
            })
            .shutdown(ShutdownPolicy::new(
                Duration::from_secs(5),
                ShutdownMode::CooperativeStrict,
            )),
        )
        .build()
        .expect("valid supervisor")
        .spawn();

    let remover = handle.clone();
    let removal = tokio::spawn(async move { remover.remove_child("worker").await });

    let mut snapshots = handle.subscribe_snapshots();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot
            .child("worker")
            .is_some_and(|child| child.membership == ChildMembershipView::Removing)
    })
    .await;

    let error = handle
        .monitor_restart("worker")
        .expect_err("constructor should reject a child in the removal window");
    assert_eq!(
        error,
        RestartMonitorError::ChildRemoved("worker".to_owned())
    );

    removal.abort();
}

#[tokio::test]
async fn monitor_restart_unknown_child_errors_immediately() {
    let handle = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor")
        .spawn();

    let error = handle
        .monitor_restart("nope")
        .expect_err("unknown child should fail synchronously");
    assert_eq!(error, RestartMonitorError::UnknownChild("nope".to_owned()));

    shutdown(handle).await;
}

fn fail_on_generations(
    id: &'static str,
    trigger_failure: Arc<Notify>,
    generations_to_fail: u64,
) -> ChildSpec {
    ChildSpec::new(id, move |ctx| {
        let trigger_failure = trigger_failure.clone();
        async move {
            if ctx.generation() < generations_to_fail {
                trigger_failure.notified().await;
                return Err(common::test_error("boom"));
            }

            ctx.shutdown_token().cancelled().await;
            Ok(())
        }
    })
    .restart(RestartPolicy::OnFailure)
}

async fn wait_for_child_running(
    snapshots: &mut watch::Receiver<SupervisorSnapshot>,
    id: &str,
    generation: u64,
) -> ChildSnapshot {
    wait_for_snapshot(snapshots, |snapshot| {
        snapshot.child(id).is_some_and(|child| {
            child.generation == generation && child.state == ChildStateView::Running
        })
    })
    .await
    .child(id)
    .expect("child should exist in matching snapshot")
    .clone()
}

async fn wait_for_snapshot(
    snapshots: &mut watch::Receiver<SupervisorSnapshot>,
    predicate: impl Fn(&SupervisorSnapshot) -> bool,
) -> SupervisorSnapshot {
    if predicate(&snapshots.borrow()) {
        return snapshots.borrow().clone();
    }

    loop {
        timeout(common::EVENT_TIMEOUT, snapshots.changed())
            .await
            .expect("timed out waiting for snapshot update")
            .expect("snapshot stream closed unexpectedly");

        let snapshot = snapshots.borrow().clone();
        if predicate(&snapshot) {
            return snapshot;
        }
    }
}

async fn shutdown(handle: tokio_supervisor::SupervisorHandle) {
    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}
