use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::{
    sync::{Barrier, Notify, mpsc},
    time::{Duration, timeout},
};
use tokio_supervisor::{
    BackoffPolicy, ChildSpec, Restart, RestartIntensity, Strategy, SupervisorBuilder,
    SupervisorExit,
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
                return Err(common::test_error("restart group"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let peer = ChildSpec::new("peer", move |ctx| {
        let peer_tx = peer_tx.clone();
        async move {
            peer_tx.send(ctx.generation).expect("test receiver dropped");
            ctx.token.cancelled().await;
            Ok(())
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
            Ok(())
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
                return Err(common::test_error("restart group"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let peer = ChildSpec::new("peer", move |ctx| {
        let peer_tx = peer_tx.clone();
        async move {
            peer_tx.send(ctx.generation).expect("test receiver dropped");
            ctx.token.cancelled().await;
            Ok(())
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
                return Err(common::test_error("restart group"));
            }

            ctx.token.cancelled().await;
            Ok(())
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
            Ok(())
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

#[tokio::test]
async fn drained_old_generation_failure_does_not_poison_later_completed_exit() {
    let release_failure = Arc::new(Notify::new());
    let finish_generation_one = Arc::new(Barrier::new(3));
    let trigger_attempts = Arc::new(AtomicUsize::new(0));
    let peer_attempts = Arc::new(AtomicUsize::new(0));

    let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();

    let release_failure_for_child = release_failure.clone();
    let finish_generation_one_for_trigger = finish_generation_one.clone();
    let trigger = ChildSpec::new("trigger", move |ctx| {
        let release_failure = release_failure_for_child.clone();
        let finish_generation_one = finish_generation_one_for_trigger.clone();
        let trigger_attempts = trigger_attempts.clone();
        let trigger_tx = trigger_tx.clone();
        async move {
            trigger_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if trigger_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                release_failure.notified().await;
                return Err(common::test_error("restart group"));
            }

            finish_generation_one.wait().await;
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let finish_generation_one_for_peer = finish_generation_one.clone();
    let peer = ChildSpec::new("peer", move |ctx| {
        let finish_generation_one = finish_generation_one_for_peer.clone();
        let peer_attempts = peer_attempts.clone();
        let peer_tx = peer_tx.clone();
        async move {
            peer_tx.send(ctx.generation).expect("test receiver dropped");
            if peer_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                ctx.token.cancelled().await;
                return Err(common::test_error("drained old generation"));
            }

            finish_generation_one.wait().await;
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .child(trigger)
        .child(peer)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_event(&mut trigger_rx).await, 0);
    assert_eq!(common::recv_event(&mut peer_rx).await, 0);

    release_failure.notify_one();

    assert_eq!(common::recv_event(&mut trigger_rx).await, 1);
    assert_eq!(common::recv_event(&mut peer_rx).await, 1);

    finish_generation_one.wait().await;

    let exit = timeout(Duration::from_secs(1), handle.wait())
        .await
        .expect("supervisor should exit after generation one completes")
        .expect("supervisor should exit cleanly");
    assert!(matches!(exit, SupervisorExit::Completed));
}

#[tokio::test]
async fn group_restart_uses_the_failing_child_restart_intensity() {
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
                return Err(common::test_error("restart group"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient)
    .restart_intensity(RestartIntensity {
        max_restarts: 1,
        within: Duration::from_secs(1),
        backoff: BackoffPolicy::None,
    });

    let peer = ChildSpec::new("peer", move |ctx| {
        let peer_tx = peer_tx.clone();
        async move {
            peer_tx.send(ctx.generation).expect("test receiver dropped");
            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Permanent)
    .restart_intensity(RestartIntensity {
        max_restarts: 0,
        within: Duration::from_secs(1),
        backoff: BackoffPolicy::None,
    });

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForAll)
        .restart_intensity(RestartIntensity {
            max_restarts: 0,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
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
