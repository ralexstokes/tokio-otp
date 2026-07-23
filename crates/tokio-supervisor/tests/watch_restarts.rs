use std::{sync::Arc, time::Duration};

use tokio::{
    sync::{Notify, watch},
    time::timeout,
};
use tokio_supervisor::{
    ChildSpec, ChildStateView, RestartIntensity, RestartPolicy, Strategy, SupervisorBuilder,
    SupervisorSnapshot,
};

mod common;

#[tokio::test]
async fn watch_restarts_reports_each_restart_as_a_delta() {
    let trigger_failure = Arc::new(Notify::new());
    let child = fail_on_generations("worker", trigger_failure.clone(), 3);
    let handle = SupervisorBuilder::new()
        .child(child)
        .build()
        .expect("valid supervisor")
        .spawn();

    let mut restarts = handle.watch_restarts();
    assert_eq!(restarts.observed(), 0);

    let mut snapshots = handle.subscribe_snapshots();
    for generation in 1..=3 {
        trigger_failure.notify_one();
        let delta = timeout(common::EVENT_TIMEOUT, restarts.next())
            .await
            .expect("restart watch should observe the restart")
            .expect("supervisor is still running");
        assert_eq!(delta, 1);
        wait_for_child_running(&mut snapshots, "worker", generation).await;
    }
    assert_eq!(restarts.observed(), 3);
    assert_eq!(handle.snapshot().total_restarts, 3);

    shutdown(handle).await;
}

#[tokio::test]
async fn watch_restarts_baseline_excludes_prior_restarts() {
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

    let mut restarts = handle.watch_restarts();
    assert_eq!(restarts.observed(), 1);
    timeout(common::QUIET_TIMEOUT, restarts.next())
        .await
        .expect_err("watch must not report restarts recorded before creation");

    trigger_failure.notify_one();
    let delta = timeout(common::EVENT_TIMEOUT, restarts.next())
        .await
        .expect("restart watch should observe the restart")
        .expect("supervisor is still running");
    assert_eq!(delta, 1);

    shutdown(handle).await;
}

#[tokio::test]
async fn watch_restarts_ends_after_supervisor_stops() {
    let handle = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor")
        .spawn();

    let mut restarts = handle.watch_restarts();
    shutdown(handle).await;

    let observed = timeout(common::EVENT_TIMEOUT, restarts.next())
        .await
        .expect("restart watch should resolve after shutdown");
    assert_eq!(observed, None);
}

#[tokio::test]
async fn total_restarts_survives_child_removal() {
    let trigger_failure = Arc::new(Notify::new());
    let child = fail_on_generations("flaky", trigger_failure.clone(), 2);
    let handle = SupervisorBuilder::new()
        .child(child)
        .child(ChildSpec::new("steady", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor")
        .spawn();

    let mut snapshots = handle.subscribe_snapshots();
    for generation in 1..=2 {
        trigger_failure.notify_one();
        wait_for_child_running(&mut snapshots, "flaky", generation).await;
    }

    handle
        .remove_child("flaky")
        .await
        .expect("child removal should succeed");
    let snapshot =
        wait_for_snapshot(&mut snapshots, |snapshot| snapshot.child("flaky").is_none()).await;

    // The removed child's per-child count is gone, but the supervisor-level
    // cumulative counter keeps every recorded restart.
    let per_child_sum: u64 = snapshot
        .children
        .iter()
        .map(|child| child.restart_count)
        .sum();
    assert_eq!(per_child_sum, 0);
    assert_eq!(snapshot.total_restarts, 2);

    shutdown(handle).await;
}

#[tokio::test]
async fn one_for_all_sibling_respawns_do_not_count_as_restarts() {
    let trigger_failure = Arc::new(Notify::new());
    let failing = fail_on_generations("failing", trigger_failure.clone(), 1)
        .restart_intensity(RestartIntensity::new(5, Duration::from_secs(1)));
    let handle = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .child(failing)
        .child(
            ChildSpec::new("sibling", |ctx| async move {
                ctx.shutdown_token().cancelled().await;
                Ok(())
            })
            .restart(RestartPolicy::OnFailure),
        )
        .build()
        .expect("valid supervisor")
        .spawn();

    let mut snapshots = handle.subscribe_snapshots();
    trigger_failure.notify_one();
    wait_for_child_running(&mut snapshots, "failing", 1).await;
    wait_for_child_running(&mut snapshots, "sibling", 1).await;

    // Both children were respawned, but only the failing child's restart is a
    // failure-triggered restart; the counter matches the intensity-window
    // semantics rather than the number of `ChildRestarted` events.
    assert_eq!(handle.snapshot().total_restarts, 1);

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
) {
    wait_for_snapshot(snapshots, |snapshot| {
        snapshot.child(id).is_some_and(|child| {
            child.generation == generation && child.state == ChildStateView::Running
        })
    })
    .await;
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
