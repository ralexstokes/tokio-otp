use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{Notify, mpsc};
use tokio_supervisor::{
    BackoffPolicy, ChildSpec, Restart, RestartIntensity, SupervisorBuilder, SupervisorError,
    SupervisorExit,
};

mod common;

#[tokio::test]
async fn repeated_failures_can_exceed_restart_intensity() {
    let supervisor = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
        .child(
            ChildSpec::new("flaky", |_| async { Err(common::test_error("boom")) })
                .restart(Restart::Transient),
        )
        .build()
        .expect("valid supervisor");

    let err = supervisor
        .run()
        .await
        .expect_err("supervisor should fail once restart intensity is exceeded");

    assert!(matches!(err, SupervisorError::RestartIntensityExceeded));
}

#[tokio::test]
async fn configured_backoff_delays_restart_attempts() {
    let supervisor = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::Fixed(Duration::from_millis(75)),
        })
        .child(
            ChildSpec::new("flaky", |_| async { Err(common::test_error("boom")) })
                .restart(Restart::Transient),
        )
        .build()
        .expect("valid supervisor");

    let started = Instant::now();
    let err = supervisor
        .run()
        .await
        .expect_err("supervisor should eventually fail once restart intensity is exceeded");

    assert!(matches!(err, SupervisorError::RestartIntensityExceeded));
    assert!(
        started.elapsed() >= Duration::from_millis(75),
        "fixed backoff should delay the first restart attempt"
    );
}

#[tokio::test]
async fn child_restart_intensity_override_controls_backoff() {
    let supervisor = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 0,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
        .child(
            ChildSpec::new("flaky", |_| async { Err(common::test_error("boom")) })
                .restart(Restart::Transient)
                .restart_intensity(RestartIntensity {
                    max_restarts: 1,
                    within: Duration::from_secs(1),
                    backoff: BackoffPolicy::Fixed(Duration::from_millis(75)),
                }),
        )
        .build()
        .expect("valid supervisor");

    let started = Instant::now();
    let err = supervisor
        .run()
        .await
        .expect_err("supervisor should eventually fail once the child override is exceeded");

    assert!(matches!(err, SupervisorError::RestartIntensityExceeded));
    assert!(
        started.elapsed() >= Duration::from_millis(75),
        "child-specific backoff should delay the first restart attempt"
    );
}

#[tokio::test]
async fn restart_intensity_is_tracked_per_child_for_one_for_one() {
    let alpha_attempts = Arc::new(AtomicUsize::new(0));
    let beta_attempts = Arc::new(AtomicUsize::new(0));
    let (alpha_tx, mut alpha_rx) = mpsc::unbounded_channel();
    let (beta_tx, mut beta_rx) = mpsc::unbounded_channel();

    let alpha = ChildSpec::new("alpha", move |ctx| {
        let alpha_attempts = alpha_attempts.clone();
        let alpha_tx = alpha_tx.clone();
        async move {
            alpha_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if alpha_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(common::test_error("alpha boom"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let beta = ChildSpec::new("beta", move |ctx| {
        let beta_attempts = beta_attempts.clone();
        let beta_tx = beta_tx.clone();
        async move {
            beta_tx.send(ctx.generation).expect("test receiver dropped");
            if beta_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(common::test_error("beta boom"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let handle = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
        .child(alpha)
        .child(beta)
        .build()
        .expect("valid supervisor")
        .spawn();

    assert_eq!(common::recv_n(&mut alpha_rx, 2).await, vec![0, 1]);
    assert_eq!(common::recv_n(&mut beta_rx, 2).await, vec![0, 1]);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn child_restart_intensity_override_is_enforced() {
    let (starts_tx, mut starts_rx) = mpsc::unbounded_channel();

    let handle = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 10,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
        .child(
            ChildSpec::new("flaky", move |ctx| {
                let starts_tx = starts_tx.clone();
                async move {
                    starts_tx
                        .send(ctx.generation)
                        .expect("test receiver dropped");
                    Err(common::test_error("boom"))
                }
            })
            .restart(Restart::Transient)
            .restart_intensity(RestartIntensity {
                max_restarts: 1,
                within: Duration::from_secs(1),
                backoff: BackoffPolicy::None,
            }),
        )
        .build()
        .expect("valid supervisor")
        .spawn();

    assert_eq!(common::recv_n(&mut starts_rx, 2).await, vec![0, 1]);
    common::assert_no_event(&mut starts_rx).await;

    let err = handle
        .wait()
        .await
        .expect_err("child-specific restart limit should fail the supervisor");

    assert!(matches!(err, SupervisorError::RestartIntensityExceeded));
}

#[tokio::test]
async fn restart_intensity_is_tracked_per_failing_child_for_one_for_all() {
    let release_alpha = Arc::new(Notify::new());
    let release_beta = Arc::new(Notify::new());
    let (alpha_tx, mut alpha_rx) = mpsc::unbounded_channel();
    let (beta_tx, mut beta_rx) = mpsc::unbounded_channel();

    let release_alpha_for_child = release_alpha.clone();
    let alpha = ChildSpec::new("alpha", move |ctx| {
        let release_alpha = release_alpha_for_child.clone();
        let alpha_tx = alpha_tx.clone();
        async move {
            alpha_tx
                .send(ctx.generation)
                .expect("test receiver dropped");
            if ctx.generation == 0 {
                release_alpha.notified().await;
                return Err(common::test_error("alpha boom"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let release_beta_for_child = release_beta.clone();
    let beta = ChildSpec::new("beta", move |ctx| {
        let release_beta = release_beta_for_child.clone();
        let beta_tx = beta_tx.clone();
        async move {
            beta_tx.send(ctx.generation).expect("test receiver dropped");
            if ctx.generation == 1 {
                release_beta.notified().await;
                return Err(common::test_error("beta boom"));
            }

            ctx.token.cancelled().await;
            Ok(())
        }
    })
    .restart(Restart::Transient);

    let handle = SupervisorBuilder::new()
        .strategy(tokio_supervisor::Strategy::OneForAll)
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::None,
        })
        .child(alpha)
        .child(beta)
        .build()
        .expect("valid supervisor")
        .spawn();

    assert_eq!(common::recv_event(&mut alpha_rx).await, 0);
    assert_eq!(common::recv_event(&mut beta_rx).await, 0);

    release_alpha.notify_one();

    assert_eq!(common::recv_event(&mut alpha_rx).await, 1);
    assert_eq!(common::recv_event(&mut beta_rx).await, 1);

    release_beta.notify_one();

    assert_eq!(common::recv_event(&mut alpha_rx).await, 2);
    assert_eq!(common::recv_event(&mut beta_rx).await, 2);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}
