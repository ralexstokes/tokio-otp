use std::time::Duration;

use tokio_supervisor::{
    BackoffPolicy, BuildError, ChildResult, ChildSpec, RestartIntensity, SupervisorBuilder,
};

#[test]
fn empty_children_are_rejected() {
    let err = SupervisorBuilder::new()
        .build()
        .expect_err("building without any children must fail");

    assert!(matches!(err, BuildError::EmptyChildren));
}

#[test]
fn duplicate_child_ids_are_rejected() {
    let err = SupervisorBuilder::new()
        .child(ChildSpec::new("dup", |_| async { ChildResult::Completed }))
        .child(ChildSpec::new("dup", |_| async { ChildResult::Completed }))
        .build()
        .expect_err("duplicate child ids must be rejected");

    assert!(matches!(err, BuildError::DuplicateChildId(id) if id == "dup"));
}

#[test]
fn invalid_restart_intensity_is_rejected() {
    let err = SupervisorBuilder::new()
        .restart_intensity(RestartIntensity {
            max_restarts: 1,
            within: Duration::ZERO,
            backoff: BackoffPolicy::None,
        })
        .child(ChildSpec::new("worker", |_| async {
            ChildResult::Completed
        }))
        .build()
        .expect_err("zero-width restart windows should be rejected");

    assert!(matches!(err, BuildError::InvalidConfig(_)));
}

#[test]
fn valid_configuration_builds() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("worker", |_| async {
            ChildResult::Completed
        }))
        .build();

    assert!(supervisor.is_ok(), "expected valid configuration to build");
}
