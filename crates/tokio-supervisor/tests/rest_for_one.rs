use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::sync::{Notify, mpsc};
use tokio_supervisor::{ChildSpec, RestartPolicy, Strategy, SupervisorBuilder};

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
