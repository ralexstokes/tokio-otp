use std::time::{Duration, Instant};

use tokio_supervisor::{
    BackoffPolicy, ChildResult, ChildSpec, Restart, RestartIntensity, SupervisorBuilder,
    SupervisorError,
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
            ChildSpec::new("flaky", |_| async {
                ChildResult::Failed(common::test_error("boom"))
            })
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
            ChildSpec::new("flaky", |_| async {
                ChildResult::Failed(common::test_error("boom"))
            })
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
