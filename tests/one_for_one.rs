use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::{
    sync::mpsc,
    time::{Duration, sleep, timeout},
};
use tokio_supervisor::{
    ChildResult, ChildSpec, Restart, Strategy, SupervisorBuilder, SupervisorExit,
};

mod common;

#[tokio::test]
async fn failed_transient_child_restarts_and_sibling_keeps_running() {
    let (flaky_tx, mut flaky_rx) = mpsc::unbounded_channel();
    let (sibling_tx, mut sibling_rx) = mpsc::unbounded_channel();
    let sibling_ticks = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));

    let flaky_attempts = attempts.clone();
    let flaky = ChildSpec::new("flaky", move |ctx| {
        let flaky_attempts = flaky_attempts.clone();
        let flaky_tx = flaky_tx.clone();
        async move {
            flaky_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if flaky_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return ChildResult::Failed(common::test_error("boom"));
            }

            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Transient);

    let sibling_ticks_for_child = sibling_ticks.clone();
    let sibling = ChildSpec::new("sibling", move |ctx| {
        let sibling_ticks_for_child = sibling_ticks_for_child.clone();
        let sibling_tx = sibling_tx.clone();
        async move {
            sibling_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            loop {
                tokio::select! {
                    _ = ctx.token.cancelled() => return ChildResult::Completed,
                    _ = sleep(Duration::from_millis(10)) => {
                        sibling_ticks_for_child.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
    });

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForOne)
        .child(flaky)
        .child(sibling)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_n(&mut flaky_rx, 2).await, vec![0, 1]);
    assert_eq!(common::recv_event(&mut sibling_rx).await, 0);
    common::assert_no_event(&mut sibling_rx).await;

    timeout(Duration::from_secs(1), async {
        while sibling_ticks.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sibling should keep running while flaky child restarts");

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn permanent_child_restarts_after_completion() {
    let (starts_tx, mut starts_rx) = mpsc::unbounded_channel();
    let attempts = Arc::new(AtomicUsize::new(0));

    let child = ChildSpec::new("permanent", move |ctx| {
        let attempts = attempts.clone();
        let starts_tx = starts_tx.clone();
        async move {
            starts_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return ChildResult::Completed;
            }

            ctx.token.cancelled().await;
            ChildResult::Completed
        }
    })
    .restart(Restart::Permanent);

    let supervisor = SupervisorBuilder::new()
        .child(child)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_n(&mut starts_rx, 2).await, vec![0, 1]);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn temporary_child_does_not_restart() {
    let (starts_tx, mut starts_rx) = mpsc::unbounded_channel();

    let supervisor = SupervisorBuilder::new()
        .child(
            ChildSpec::new("temporary", move |ctx| {
                let starts_tx = starts_tx.clone();
                async move {
                    starts_tx
                        .send(ctx.generation)
                        .expect("test receiver dropped");
                    ChildResult::Failed(common::test_error("no restart"))
                }
            })
            .restart(Restart::Temporary),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_event(&mut starts_rx).await, 0);
    let _ = timeout(Duration::from_secs(1), handle.wait())
        .await
        .expect("supervisor should finish once its only temporary child stops")
        .expect("supervisor should exit cleanly");

    common::assert_no_event(&mut starts_rx).await;
}
