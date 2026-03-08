use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_supervisor::{
    ChildResult, ChildSpec, ShutdownMode, ShutdownPolicy, SupervisorBuilder, SupervisorExit,
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
                ctx.token.cancelled().await;
                exits.fetch_add(1, Ordering::SeqCst);
                ChildResult::Completed
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

    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
    assert_eq!(exits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn shutdown_is_idempotent_across_handle_clones() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |ctx| async move {
            ctx.token.cancelled().await;
            ChildResult::Completed
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let clone = handle.clone();

    handle.shutdown();
    clone.shutdown();
    handle.shutdown();

    let first = handle.wait().await.expect("first waiter should resolve");
    let second = clone.wait().await.expect("second waiter should resolve");

    assert!(matches!(first, SupervisorExit::Shutdown));
    assert!(matches!(second, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn cooperative_child_observes_cancellation_before_shutdown_finishes() {
    let saw_cancel = Arc::new(AtomicBool::new(false));

    let saw_cancel_for_child = saw_cancel.clone();
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", move |ctx| {
            let saw_cancel = saw_cancel_for_child.clone();
            async move {
                ctx.token.cancelled().await;
                saw_cancel.store(true, Ordering::SeqCst);
                ChildResult::Completed
            }
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    handle.shutdown();

    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
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
            .shutdown_policy(ShutdownPolicy {
                grace: common::SHORT_GRACE,
                mode: ShutdownMode::CooperativeThenAbort,
            }),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    handle.shutdown();

    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
    assert!(!saw_cancel.load(Ordering::SeqCst));
    assert!(
        !live_flag.is_live(),
        "task should be dropped before wait resolves"
    );
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
            .shutdown_policy(ShutdownPolicy {
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
    let exit = handle.wait().await.expect("shutdown should succeed");

    assert!(matches!(exit, SupervisorExit::Shutdown));
    assert!(
        !live_flag.is_live(),
        "child must be dropped before wait completes"
    );
}
