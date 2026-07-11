use std::time::Duration;

use tokio_supervisor::{
    AutoShutdown, BackoffPolicy, ChildSpec, RestartIntensity, RestartPolicy, SupervisorBuildError,
    SupervisorBuilder,
};

#[test]
fn empty_children_are_accepted() {
    SupervisorBuilder::new()
        .build()
        .expect("building without children should succeed");
}

#[test]
fn significant_always_child_is_rejected() {
    let err = SupervisorBuilder::new()
        .auto_shutdown(AutoShutdown::AnySignificant)
        .child(
            ChildSpec::new("worker", |_| async { Ok(()) })
                .restart(RestartPolicy::Always)
                .significant(),
        )
        .build()
        .expect_err("significant permanent children must be rejected");

    assert_eq!(
        err,
        SupervisorBuildError::InvalidConfig(
            "significant children cannot use RestartPolicy::Always"
        )
    );
}

#[test]
fn significant_child_requires_automatic_shutdown() {
    let err = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |_| async { Ok(()) }).significant())
        .build()
        .expect_err("significant child without automatic shutdown must be rejected");

    assert_eq!(
        err,
        SupervisorBuildError::InvalidConfig("significant children require automatic shutdown")
    );
}

#[test]
fn duplicate_child_ids_are_rejected() {
    let err = SupervisorBuilder::new()
        .child(ChildSpec::new("dup", |_| async { Ok(()) }))
        .child(ChildSpec::new("dup", |_| async { Ok(()) }))
        .build()
        .expect_err("duplicate child ids must be rejected");

    assert!(matches!(err, SupervisorBuildError::DuplicateChildId(id) if id == "dup"));
}

#[test]
fn invalid_restart_intensity_is_rejected() {
    let err = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::ZERO,
            backoff: BackoffPolicy::None,
        })
        .child(ChildSpec::new("worker", |_| async { Ok(()) }))
        .build()
        .expect_err("zero-width restart windows should be rejected");

    assert!(matches!(err, SupervisorBuildError::InvalidConfig(_)));
}

#[test]
fn invalid_jittered_restart_intensity_is_rejected() {
    let err = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::JitteredExponential {
                base: Duration::ZERO,
                factor: 2,
                max: Duration::from_millis(10),
            },
        })
        .child(ChildSpec::new("worker", |_| async { Ok(()) }))
        .build()
        .expect_err("invalid jittered exponential backoff should be rejected");

    assert!(matches!(err, SupervisorBuildError::InvalidConfig(_)));
}

#[test]
fn invalid_fixed_backoff_delay_is_rejected() {
    let err = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::Fixed(Duration::ZERO),
        })
        .child(ChildSpec::new("worker", |_| async { Ok(()) }))
        .build()
        .expect_err("zero fixed backoff delay should be rejected");

    assert!(matches!(err, SupervisorBuildError::InvalidConfig(_)));
}

#[test]
fn invalid_exponential_restart_factor_is_rejected() {
    let err = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::Exponential {
                base: Duration::from_millis(10),
                factor: 0,
                max: Duration::from_millis(20),
            },
        })
        .child(ChildSpec::new("worker", |_| async { Ok(()) }))
        .build()
        .expect_err("zero exponential factor should be rejected");

    assert!(matches!(err, SupervisorBuildError::InvalidConfig(_)));
}

#[test]
fn invalid_exponential_restart_max_is_rejected() {
    let err = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::from_secs(1),
            backoff: BackoffPolicy::Exponential {
                base: Duration::from_millis(10),
                factor: 2,
                max: Duration::ZERO,
            },
        })
        .child(ChildSpec::new("worker", |_| async { Ok(()) }))
        .build()
        .expect_err("zero exponential max should be rejected");

    assert!(matches!(err, SupervisorBuildError::InvalidConfig(_)));
}

#[test]
fn invalid_child_restart_intensity_is_rejected() {
    let err = SupervisorBuilder::new()
        .child(
            ChildSpec::new("worker", |_| async { Ok(()) }).restart_intensity(RestartIntensity {
                max_restarts: 1,
                within: Duration::ZERO,
                backoff: BackoffPolicy::None,
            }),
        )
        .build()
        .expect_err("zero-width child restart windows should be rejected");

    assert!(matches!(err, SupervisorBuildError::InvalidConfig(_)));
}

#[test]
fn empty_child_id_is_rejected() {
    let err = SupervisorBuilder::new()
        .child(ChildSpec::new("", |_| async { Ok(()) }))
        .build()
        .expect_err("empty child id must be rejected");

    assert!(matches!(err, SupervisorBuildError::InvalidConfig(_)));
}

#[test]
fn zero_channel_capacities_are_rejected() {
    let control_err = SupervisorBuilder::new()
        .control_channel_capacity(0)
        .child(ChildSpec::new("worker", |_| async { Ok(()) }))
        .build()
        .expect_err("zero control channel capacity must be rejected");
    assert!(matches!(
        control_err,
        SupervisorBuildError::InvalidConfig(_)
    ));

    let event_err = SupervisorBuilder::new()
        .event_channel_capacity(0)
        .child(ChildSpec::new("worker", |_| async { Ok(()) }))
        .build()
        .expect_err("zero event channel capacity must be rejected");
    assert!(matches!(event_err, SupervisorBuildError::InvalidConfig(_)));
}

#[test]
fn valid_configuration_builds() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |_| async { Ok(()) }))
        .build();

    assert!(supervisor.is_ok(), "expected valid configuration to build");
}
