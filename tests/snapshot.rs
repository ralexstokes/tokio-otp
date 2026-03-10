use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, mpsc, watch},
    time::timeout,
};
use tokio_supervisor::{
    BackoffPolicy, ChildMembershipView, ChildSnapshot, ChildSpec, ChildStateView, ExitStatusView,
    Restart, RestartIntensity, SupervisorBuilder, SupervisorEvent, SupervisorExit,
    SupervisorSnapshot, SupervisorStateView,
};

mod common;

#[tokio::test]
async fn initial_snapshot_is_immediately_available_and_preserves_child_order() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("alpha", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .child(ChildSpec::new("beta", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let snapshot = handle.snapshot();

    assert_eq!(snapshot.state, SupervisorStateView::Running);
    assert_eq!(snapshot.last_exit, None);
    assert_eq!(child_ids(&snapshot), vec!["alpha", "beta"]);
    for entry in &snapshot.children {
        assert_eq!(entry.membership, ChildMembershipView::Active);
        assert_eq!(entry.last_exit, None);
        assert_eq!(entry.restart_count, 0);
        assert_eq!(entry.next_restart_in, None);
    }

    handle
        .add_child(ChildSpec::new("gamma", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .await
        .expect("dynamic child should be accepted");

    let mut snapshots = handle.subscribe_snapshots();
    let updated = wait_for_snapshot(&mut snapshots, |snapshot| {
        child_ids(snapshot) == vec!["alpha", "beta", "gamma"]
    })
    .await;
    assert_eq!(child_ids(&updated), vec!["alpha", "beta", "gamma"]);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert_eq!(exit, SupervisorExit::Shutdown);
}

#[tokio::test]
async fn snapshot_shows_restart_state_and_last_exit() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let (starts_tx, mut starts_rx) = mpsc::unbounded_channel();

    let flaky_child = ChildSpec::new("flaky", move |ctx| {
        let attempts = attempts.clone();
        let starts_tx = starts_tx.clone();
        async move {
            starts_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(common::test_error("boom"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient)
    .restart_intensity(RestartIntensity {
        max_restarts: 5,
        within: Duration::from_secs(1),
        backoff: BackoffPolicy::Fixed(Duration::from_millis(200)),
    });

    let supervisor = SupervisorBuilder::new()
        .child(flaky_child)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut snapshots = handle.subscribe_snapshots();

    assert_eq!(common::recv_event(&mut starts_rx).await, 0);

    let restarting = wait_for_snapshot(&mut snapshots, |snapshot| {
        child(snapshot, "flaky").is_some_and(|child| {
            child.state == ChildStateView::Stopped
                && child.restart_count == 1
                && matches!(
                    child.last_exit.as_ref(),
                    Some(ExitStatusView::Failed(message)) if message.contains("boom")
                )
                && child.next_restart_in.is_some()
        })
    })
    .await;
    let flaky = child(&restarting, "flaky").expect("flaky child should exist");
    assert!(
        flaky
            .next_restart_in
            .expect("restart delay should be visible")
            <= Duration::from_millis(200)
    );

    assert_eq!(common::recv_event(&mut starts_rx).await, 1);
    let running_again = wait_for_snapshot(&mut snapshots, |snapshot| {
        child(snapshot, "flaky").is_some_and(|child| {
            child.generation == 1
                && child.state == ChildStateView::Running
                && child.restart_count == 1
                && child.next_restart_in.is_none()
        })
    })
    .await;
    assert!(matches!(
        child(&running_again, "flaky")
            .expect("flaky child should exist")
            .last_exit
            .as_ref(),
        Some(ExitStatusView::Failed(message)) if message.contains("boom")
    ));

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert_eq!(exit, SupervisorExit::Shutdown);
}

#[tokio::test]
async fn snapshot_shows_removing_membership_during_child_removal() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());

    let release_for_child = release.clone();
    let removable = ChildSpec::new("removable", move |ctx| {
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
    });

    let supervisor = SupervisorBuilder::new()
        .child(removable)
        .child(ChildSpec::new("keeper", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut snapshots = handle.subscribe_snapshots();

    common::recv_event(&mut started_rx).await;

    let remove_handle = handle.clone();
    let remove_task = tokio::spawn(async move { remove_handle.remove_child("removable").await });

    common::recv_event(&mut cancelled_rx).await;

    let removing = wait_for_snapshot(&mut snapshots, |snapshot| {
        child(snapshot, "removable").is_some_and(|entry| {
            entry.membership == ChildMembershipView::Removing
                && entry.state == ChildStateView::Stopping
        })
    })
    .await;
    let removable = child(&removing, "removable").expect("removable child should exist");
    assert_eq!(removable.membership, ChildMembershipView::Removing);
    assert_eq!(removable.state, ChildStateView::Stopping);

    release.notify_one();
    remove_task
        .await
        .expect("remove task should join")
        .expect("child removal should succeed");

    let removed = wait_for_snapshot(&mut snapshots, |snapshot| {
        child(snapshot, "removable").is_none() && child(snapshot, "keeper").is_some()
    })
    .await;
    assert!(child(&removed, "removable").is_none());

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert_eq!(exit, SupervisorExit::Shutdown);
}

#[tokio::test]
async fn root_snapshot_includes_nested_supervisor_tree() {
    let (leaf_started_tx, mut leaf_started_rx) = mpsc::unbounded_channel();

    let nested = SupervisorBuilder::new()
        .child(ChildSpec::new("leaf", move |ctx| {
            let leaf_started_tx = leaf_started_tx.clone();
            async move {
                leaf_started_tx.send(()).expect("test receiver dropped");
                ctx.token.cancelled().await;
                Ok(())
            }
        }))
        .build()
        .expect("valid nested supervisor");

    let outer = SupervisorBuilder::new()
        .child(ChildSpec::new("anchor", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .child(nested.into_child_spec("nested"))
        .build()
        .expect("valid outer supervisor");

    let handle = outer.spawn();
    let mut snapshots = handle.subscribe_snapshots();

    common::recv_event(&mut leaf_started_rx).await;

    let snapshot = wait_for_snapshot(&mut snapshots, |snapshot| {
        child(snapshot, "nested").is_some_and(|entry| {
            entry.state == ChildStateView::Running
                && entry.supervisor.as_ref().is_some_and(|nested| {
                    nested.state == SupervisorStateView::Running
                        && child(nested, "leaf").is_some_and(|leaf| {
                            leaf.state == ChildStateView::Running
                                && leaf.membership == ChildMembershipView::Active
                        })
                })
        })
    })
    .await;
    let nested = child(&snapshot, "nested")
        .expect("nested supervisor child should exist")
        .supervisor
        .as_ref()
        .expect("nested snapshot should be present");
    assert_eq!(nested.state, SupervisorStateView::Running);
    assert!(child(nested, "leaf").is_some());

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert_eq!(exit, SupervisorExit::Shutdown);
}

#[tokio::test]
async fn stopped_snapshot_remains_available_after_shutdown() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut snapshots = handle.subscribe_snapshots();

    let _ = wait_for_snapshot(&mut snapshots, |snapshot| {
        child(snapshot, "worker").is_some_and(|child| child.state == ChildStateView::Running)
    })
    .await;

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert_eq!(exit, SupervisorExit::Shutdown);

    let snapshot = handle.snapshot();
    assert_eq!(snapshot.state, SupervisorStateView::Stopped);
    assert_eq!(snapshot.last_exit, Some(SupervisorExit::Shutdown));
    assert_eq!(
        child(&snapshot, "worker")
            .expect("worker child should remain visible")
            .state,
        ChildStateView::Stopped
    );
}

#[tokio::test]
async fn stopped_snapshot_reports_natural_completion() {
    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("temporary", |_ctx| async move { Ok(()) }).restart(Restart::Temporary),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let exit = timeout(common::EVENT_TIMEOUT, handle.wait())
        .await
        .expect("supervisor should exit")
        .expect("supervisor should complete cleanly");
    assert_eq!(exit, SupervisorExit::Completed);

    let snapshot = handle.snapshot();
    assert_eq!(snapshot.state, SupervisorStateView::Stopped);
    assert_eq!(snapshot.last_exit, Some(SupervisorExit::Completed));
    assert!(matches!(
        child(&snapshot, "temporary")
            .expect("temporary child should remain visible")
            .last_exit
            .as_ref(),
        Some(ExitStatusView::Completed)
    ));
}

#[tokio::test]
async fn events_observe_already_published_snapshot_state() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            ctx.token.cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();

    loop {
        match timeout(common::EVENT_TIMEOUT, events.recv())
            .await
            .expect("timed out waiting for supervisor event")
            .expect("supervisor event stream should remain open")
        {
            SupervisorEvent::ChildStarted { id, generation }
                if id == "worker" && generation == 0 =>
            {
                let snapshot = handle.snapshot();
                let worker = child(&snapshot, "worker").expect("worker child should exist");
                assert_eq!(snapshot.state, SupervisorStateView::Running);
                assert_eq!(worker.generation, 0);
                assert_eq!(worker.state, ChildStateView::Running);
                break;
            }
            _ => {}
        }
    }

    handle.shutdown();

    loop {
        if timeout(common::EVENT_TIMEOUT, events.recv())
            .await
            .expect("timed out waiting for supervisor event")
            .expect("supervisor event stream should remain open")
            == SupervisorEvent::SupervisorStopped
        {
            let snapshot = handle.snapshot();
            assert_eq!(snapshot.state, SupervisorStateView::Stopped);
            assert_eq!(snapshot.last_exit, Some(SupervisorExit::Shutdown));
            assert_eq!(
                child(&snapshot, "worker")
                    .expect("worker child should remain visible")
                    .state,
                ChildStateView::Stopped
            );
            break;
        }
    }

    let exit = handle.wait().await.expect("shutdown should succeed");
    assert_eq!(exit, SupervisorExit::Shutdown);
}

fn child<'a>(snapshot: &'a SupervisorSnapshot, id: &str) -> Option<&'a ChildSnapshot> {
    snapshot.children.iter().find(|child| child.id == id)
}

fn child_ids(snapshot: &SupervisorSnapshot) -> Vec<&str> {
    snapshot
        .children
        .iter()
        .map(|child| child.id.as_str())
        .collect()
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
