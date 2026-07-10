use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::{Notify, mpsc};
use tokio_supervisor::{ChildSpec, RestartPolicy, ShutdownPolicy, Strategy, SupervisorBuilder};

mod common;

#[tokio::test]
async fn middle_failure_restarts_only_the_downstream_suffix_in_order() {
    let release_failure = Arc::new(Notify::new());
    let middle_attempts = Arc::new(AtomicUsize::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let upstream = reporting_child("upstream", started_tx.clone());

    let release_failure_for_child = release_failure.clone();
    let middle_started_tx = started_tx.clone();
    let middle = ChildSpec::new("middle", move |ctx| {
        let release_failure = release_failure_for_child.clone();
        let middle_attempts = middle_attempts.clone();
        let started_tx = middle_started_tx.clone();
        async move {
            started_tx
                .send(("middle", ctx.generation()))
                .expect("test receiver dropped");
            if middle_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                release_failure.notified().await;
                return Err(common::test_error("restart downstream suffix"));
            }
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }
    })
    .restart(RestartPolicy::OnFailure);

    let downstream = reporting_child("downstream", started_tx);
    let handle = SupervisorBuilder::new()
        .strategy(Strategy::RestForOne)
        .child(upstream)
        .child(middle)
        .child(downstream)
        .build()
        .expect("valid supervisor")
        .spawn();

    assert_eq!(
        common::recv_n(&mut started_rx, 3).await,
        vec![("upstream", 0), ("middle", 0), ("downstream", 0)]
    );
    release_failure.notify_one();
    assert_eq!(
        common::recv_n(&mut started_rx, 2).await,
        vec![("middle", 1), ("downstream", 1)]
    );
    common::assert_no_event(&mut started_rx).await;

    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn last_child_failure_restarts_only_itself() {
    let release_failure = Arc::new(Notify::new());
    let last_attempts = Arc::new(AtomicUsize::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let first = reporting_child("first", started_tx.clone());
    let middle = reporting_child("middle", started_tx.clone());
    let release_failure_for_child = release_failure.clone();
    let last = ChildSpec::new("last", move |ctx| {
        let release_failure = release_failure_for_child.clone();
        let last_attempts = last_attempts.clone();
        let started_tx = started_tx.clone();
        async move {
            started_tx
                .send(("last", ctx.generation()))
                .expect("test receiver dropped");
            if last_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                release_failure.notified().await;
                return Err(common::test_error("restart last child"));
            }
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }
    })
    .restart(RestartPolicy::OnFailure);

    let handle = SupervisorBuilder::new()
        .strategy(Strategy::RestForOne)
        .child(first)
        .child(middle)
        .child(last)
        .build()
        .expect("valid supervisor")
        .spawn();

    assert_eq!(
        common::recv_n(&mut started_rx, 3).await,
        vec![("first", 0), ("middle", 0), ("last", 0)]
    );
    release_failure.notify_one();
    assert_eq!(common::recv_event(&mut started_rx).await, ("last", 1));
    common::assert_no_event(&mut started_rx).await;

    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn upstream_failure_during_suffix_drain_is_dispatched_after_the_restart() {
    let fail_upstream = Arc::new(Notify::new());
    let fail_middle = Arc::new(Notify::new());
    let slow_cancelled = Arc::new(Notify::new());
    let release_slow = Arc::new(Notify::new());
    let upstream_attempts = Arc::new(AtomicUsize::new(0));
    let middle_attempts = Arc::new(AtomicUsize::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let fail_upstream_for_child = fail_upstream.clone();
    let upstream = ChildSpec::new("upstream", {
        let started_tx = started_tx.clone();
        move |ctx| {
            let fail_upstream = fail_upstream_for_child.clone();
            let upstream_attempts = upstream_attempts.clone();
            let started_tx = started_tx.clone();
            async move {
                started_tx
                    .send(("upstream", ctx.generation()))
                    .expect("test receiver dropped");
                if upstream_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    fail_upstream.notified().await;
                    return Err(common::test_error("upstream failed during suffix drain"));
                }
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .restart(RestartPolicy::OnFailure);

    let fail_middle_for_child = fail_middle.clone();
    let middle = ChildSpec::new("middle", {
        let started_tx = started_tx.clone();
        move |ctx| {
            let fail_middle = fail_middle_for_child.clone();
            let middle_attempts = middle_attempts.clone();
            let started_tx = started_tx.clone();
            async move {
                started_tx
                    .send(("middle", ctx.generation()))
                    .expect("test receiver dropped");
                if middle_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    fail_middle.notified().await;
                    return Err(common::test_error("restart suffix"));
                }
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .restart(RestartPolicy::OnFailure);

    let slow = ChildSpec::new("slow", {
        let started_tx = started_tx.clone();
        let slow_cancelled = slow_cancelled.clone();
        let release_slow = release_slow.clone();
        move |ctx| {
            let started_tx = started_tx.clone();
            let slow_cancelled = slow_cancelled.clone();
            let release_slow = release_slow.clone();
            async move {
                started_tx
                    .send(("slow", ctx.generation()))
                    .expect("test receiver dropped");
                ctx.shutdown_token().cancelled().await;
                if ctx.generation() == 0 {
                    slow_cancelled.notify_one();
                    release_slow.notified().await;
                }
                Ok(())
            }
        }
    })
    .restart(RestartPolicy::Always)
    .shutdown(ShutdownPolicy::cooperative_then_abort(Duration::from_secs(
        1,
    )));

    let handle = SupervisorBuilder::new()
        .strategy(Strategy::RestForOne)
        .child(upstream)
        .child(middle)
        .child(slow)
        .build()
        .expect("valid supervisor")
        .spawn();

    assert_eq!(
        common::recv_n(&mut started_rx, 3).await,
        vec![("upstream", 0), ("middle", 0), ("slow", 0)]
    );
    fail_middle.notify_one();
    slow_cancelled.notified().await;
    fail_upstream.notify_one();
    tokio::task::yield_now().await;
    release_slow.notify_one();

    assert_eq!(
        common::recv_n(&mut started_rx, 5).await,
        vec![
            ("middle", 1),
            ("slow", 1),
            ("upstream", 1),
            ("middle", 2),
            ("slow", 2),
        ]
    );

    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn never_child_in_suffix_is_drained_but_not_restarted() {
    let fail_trigger = Arc::new(Notify::new());
    let trigger_attempts = Arc::new(AtomicUsize::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (never_drained_tx, mut never_drained_rx) = mpsc::unbounded_channel();

    let fail_trigger_for_child = fail_trigger.clone();
    let trigger = ChildSpec::new("trigger", {
        let started_tx = started_tx.clone();
        move |ctx| {
            let fail_trigger = fail_trigger_for_child.clone();
            let trigger_attempts = trigger_attempts.clone();
            let started_tx = started_tx.clone();
            async move {
                started_tx
                    .send(("trigger", ctx.generation()))
                    .expect("test receiver dropped");
                if trigger_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    fail_trigger.notified().await;
                    return Err(common::test_error("restart suffix"));
                }
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }
    })
    .restart(RestartPolicy::OnFailure);

    let never = ChildSpec::new("never", {
        let started_tx = started_tx.clone();
        move |ctx| {
            let started_tx = started_tx.clone();
            let never_drained_tx = never_drained_tx.clone();
            async move {
                started_tx
                    .send(("never", ctx.generation()))
                    .expect("test receiver dropped");
                ctx.shutdown_token().cancelled().await;
                never_drained_tx.send(()).expect("test receiver dropped");
                Ok(())
            }
        }
    })
    .restart(RestartPolicy::Never);
    let eligible = reporting_child("eligible", started_tx);

    let handle = SupervisorBuilder::new()
        .strategy(Strategy::RestForOne)
        .child(trigger)
        .child(never)
        .child(eligible)
        .build()
        .expect("valid supervisor")
        .spawn();

    assert_eq!(
        common::recv_n(&mut started_rx, 3).await,
        vec![("trigger", 0), ("never", 0), ("eligible", 0)]
    );
    fail_trigger.notify_one();
    common::recv_event(&mut never_drained_rx).await;
    assert_eq!(
        common::recv_n(&mut started_rx, 2).await,
        vec![("trigger", 1), ("eligible", 1)]
    );
    common::assert_no_event(&mut started_rx).await;

    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

fn reporting_child(
    id: &'static str,
    started_tx: mpsc::UnboundedSender<(&'static str, u64)>,
) -> ChildSpec {
    ChildSpec::new(id, move |ctx| {
        let started_tx = started_tx.clone();
        async move {
            started_tx
                .send((id, ctx.generation()))
                .expect("test receiver dropped");
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }
    })
    .restart(RestartPolicy::Always)
}
