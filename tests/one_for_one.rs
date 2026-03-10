use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::{
    sync::mpsc,
    time::{Duration, sleep, timeout},
};
use tokio_supervisor::{
    BackoffPolicy, ChildSpec, Restart, RestartIntensity, Strategy, SupervisorBuilder,
    SupervisorExit,
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
                return Err(common::test_error("boom"));
            }

            ctx.token.cancelled().await;
            Ok(())
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
                    _ = ctx.token.cancelled() => return Ok(()),
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
                return Ok(());
            }

            ctx.token.cancelled().await;
            Ok(())
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
                    Err(common::test_error("no restart"))
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

#[tokio::test]
async fn child_restart_intensity_is_isolated_per_child() {
    let child_restart_intensity = RestartIntensity {
        max_restarts: 1,
        within: Duration::from_secs(1),
        backoff: BackoffPolicy::None,
    };

    let (child_a_tx, mut child_a_rx) = mpsc::unbounded_channel();
    let (child_b_tx, mut child_b_rx) = mpsc::unbounded_channel();
    let child_a_attempts = Arc::new(AtomicUsize::new(0));
    let child_b_attempts = Arc::new(AtomicUsize::new(0));

    let child_a = ChildSpec::new("child-a", move |ctx| {
        let child_a_attempts = child_a_attempts.clone();
        let child_a_tx = child_a_tx.clone();
        async move {
            child_a_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if child_a_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(common::test_error("boom-a"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient)
    .restart_intensity(child_restart_intensity);

    let child_b = ChildSpec::new("child-b", move |ctx| {
        let child_b_attempts = child_b_attempts.clone();
        let child_b_tx = child_b_tx.clone();
        async move {
            child_b_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if child_b_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(common::test_error("boom-b"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient)
    .restart_intensity(child_restart_intensity);

    let supervisor = SupervisorBuilder::new()
        .strategy(Strategy::OneForOne)
        .restart_intensity(RestartIntensity {
            max_restarts: 0,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
        .child(child_a)
        .child(child_b)
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();

    assert_eq!(common::recv_n(&mut child_a_rx, 2).await, vec![0, 1]);
    assert_eq!(common::recv_n(&mut child_b_rx, 2).await, vec![0, 1]);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}
