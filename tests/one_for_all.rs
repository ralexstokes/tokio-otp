use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::sync::{Notify, mpsc};
use tokio_supervisor::{
    ChildResult, ChildSpec, Restart, Strategy, SupervisorBuilder, SupervisorExit,
};

mod common;

#[tokio::test]
async fn restartable_child_failure_restarts_the_whole_group() {
    let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
    let trigger_attempts = Arc::new(AtomicUsize::new(0));

    let trigger = ChildSpec::new("trigger", move |ctx| {
        let trigger_attempts = trigger_attempts.clone();
        let trigger_tx = trigger_tx.clone();
        async move {
            trigger_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if trigger_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return ChildResult::Failed(common::test_error("restart group"));
            }

            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Transient);

    let peer = ChildSpec::new("peer", move |ctx| {
        let peer_tx = peer_tx.clone();
        async move {
            peer_tx.send(ctx.generation).expect("test receiver dropped");
            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Permanent);

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .child(trigger)
        .child(peer)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_n(&mut trigger_rx, 2).await, vec![0, 1]);
    assert_eq!(common::recv_n(&mut peer_rx, 2).await, vec![0, 1]);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn completed_temporary_child_is_not_respawned_during_group_restart() {
    let release_failure = Arc::new(Notify::new());
    let trigger_attempts = Arc::new(AtomicUsize::new(0));

    let (temporary_tx, mut temporary_rx) = mpsc::unbounded_channel();
    let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();

    let temporary = ChildSpec::new("temporary", move |ctx| {
        let temporary_tx = temporary_tx.clone();
        async move {
            temporary_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            ChildResult::Completed
        }
    })
    .restart(Restart::Temporary);

    let release_failure_for_child = release_failure.clone();
    let trigger = ChildSpec::new("trigger", move |ctx| {
        let release_failure = release_failure_for_child.clone();
        let trigger_attempts = trigger_attempts.clone();
        let trigger_tx = trigger_tx.clone();
        async move {
            trigger_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if trigger_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                release_failure.notified().await;
                return ChildResult::Failed(common::test_error("restart group"));
            }

            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Transient);

    let peer = ChildSpec::new("peer", move |ctx| {
        let peer_tx = peer_tx.clone();
        async move {
            peer_tx.send(ctx.generation).expect("test receiver dropped");
            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Permanent);

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .child(temporary)
        .child(trigger)
        .child(peer)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_event(&mut temporary_rx).await, 0);
    assert_eq!(common::recv_event(&mut trigger_rx).await, 0);
    assert_eq!(common::recv_event(&mut peer_rx).await, 0);

    release_failure.notify_one();

    assert_eq!(common::recv_event(&mut trigger_rx).await, 1);
    assert_eq!(common::recv_event(&mut peer_rx).await, 1);
    common::assert_no_event(&mut temporary_rx).await;

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn one_for_all_does_not_overlap_old_and_new_generations() {
    let live_instances = Arc::new(AtomicUsize::new(0));
    let trigger_attempts = Arc::new(AtomicUsize::new(0));

    let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();

    let trigger = ChildSpec::new("trigger", move |ctx| {
        let trigger_attempts = trigger_attempts.clone();
        let trigger_tx = trigger_tx.clone();
        async move {
            trigger_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if trigger_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return ChildResult::Failed(common::test_error("restart group"));
            }

            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Transient);

    let peer = ChildSpec::new("peer", move |ctx| {
        let live_instances = live_instances.clone();
        let peer_tx = peer_tx.clone();
        async move {
            let active = live_instances.fetch_add(1, Ordering::SeqCst) + 1;
            peer_tx
                .send((ctx.generation, active))
                .expect("test receiver dropped");

            ctx.token.cancelled().await;
            live_instances.fetch_sub(1, Ordering::SeqCst);
            ChildResult::Completed
        }
    })
    .restart(Restart::Permanent);

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .child(trigger)
        .child(peer)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_event(&mut trigger_rx).await, 0);
    assert_eq!(common::recv_event(&mut trigger_rx).await, 1);

    let peer_events = common::recv_n(&mut peer_rx, 2).await;
    assert_eq!(peer_events, vec![(0, 1), (1, 1)]);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}
