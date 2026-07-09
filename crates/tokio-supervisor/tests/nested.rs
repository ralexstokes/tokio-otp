use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::sync::mpsc;
use tokio_supervisor::{
    ChildSpec, ControlError, EventPathSegment, ExitStatusView, Restart, SupervisorBuilder,
    SupervisorEvent, SupervisorExit,
};

mod common;

#[tokio::test]
async fn nested_supervisor_completes_as_a_clean_child_exit() {
    let nested = SupervisorBuilder::new()
        .child(ChildSpec::new("leaf", |_ctx| async move { Ok(()) }).restart(Restart::Temporary))
        .build()
        .expect("valid nested supervisor");

    let outer = SupervisorBuilder::new()
        .child(nested.into_child_spec("nested").restart(Restart::Temporary))
        .build()
        .expect("valid outer supervisor");

    let exit = outer.run().await.expect("outer supervisor should run");
    assert!(matches!(exit, SupervisorExit::Completed));
}

#[tokio::test]
async fn nested_supervisor_failure_is_visible_to_parent_restart_policy() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let (starts_tx, mut starts_rx) = mpsc::unbounded_channel();

    let nested_attempts = attempts.clone();
    let nested = SupervisorBuilder::new()
        .child(
            ChildSpec::new("leaf", move |ctx| {
                let attempts = nested_attempts.clone();
                let starts_tx = starts_tx.clone();
                async move {
                    starts_tx.send(()).expect("test receiver dropped");
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(common::test_error("nested failure"))
                    } else {
                        ctx.shutdown_token().cancelled().await;
                        Ok(())
                    }
                }
            })
            .restart(Restart::Temporary),
        )
        .build()
        .expect("valid nested supervisor");

    let outer = SupervisorBuilder::new()
        .child(nested.into_child_spec("nested").restart(Restart::Transient))
        .build()
        .expect("valid outer supervisor");

    let handle = outer.spawn();

    common::recv_event(&mut starts_rx).await;
    common::recv_event(&mut starts_rx).await;

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn parent_shutdown_propagates_into_nested_supervisor() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let cancellations = Arc::new(AtomicUsize::new(0));

    let nested_cancellations = cancellations.clone();
    let nested = SupervisorBuilder::new()
        .child(ChildSpec::new("leaf", move |ctx| {
            let started_tx = started_tx.clone();
            let cancellations = nested_cancellations.clone();
            async move {
                started_tx.send(()).expect("test receiver dropped");
                ctx.shutdown_token().cancelled().await;
                cancellations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }))
        .build()
        .expect("valid nested supervisor");

    let outer = SupervisorBuilder::new()
        .child(nested.into_child_spec("nested"))
        .build()
        .expect("valid outer supervisor");

    let handle = outer.spawn();
    common::recv_event(&mut started_rx).await;

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");

    assert!(matches!(exit, SupervisorExit::Shutdown));
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dynamically_added_nested_supervisor_can_be_removed() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let cancellations = Arc::new(AtomicUsize::new(0));

    let nested_cancellations = cancellations.clone();
    let nested = SupervisorBuilder::new()
        .child(ChildSpec::new("leaf", move |ctx| {
            let started_tx = started_tx.clone();
            let cancellations = nested_cancellations.clone();
            async move {
                started_tx.send(()).expect("test receiver dropped");
                ctx.shutdown_token().cancelled().await;
                cancellations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }))
        .build()
        .expect("valid nested supervisor");

    let outer = SupervisorBuilder::new()
        .child(ChildSpec::new("anchor", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid outer supervisor");

    let handle = outer.spawn();
    let mut events = handle.subscribe();

    handle
        .add_child(nested.into_child_spec("nested"))
        .await
        .expect("dynamic nested child should be accepted");
    common::recv_event(&mut started_rx).await;

    handle
        .remove_child("nested")
        .await
        .expect("dynamic nested child should be removable");

    let mut saw_nested_supervisor_started = false;
    let mut saw_nested_leaf_started = false;
    let mut saw_remove_requested = false;
    let mut saw_removed = false;

    while !(saw_nested_supervisor_started
        && saw_nested_leaf_started
        && saw_remove_requested
        && saw_removed)
    {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::SupervisorStarted) =>
            {
                saw_nested_supervisor_started = true;
            }
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::ChildStarted { ref id, generation: 0 } if id == "leaf") =>
            {
                saw_nested_leaf_started = true;
            }
            SupervisorEvent::ChildRemoveRequested { id } if id == "nested" => {
                saw_remove_requested = true;
            }
            SupervisorEvent::ChildRemoved { id } if id == "nested" => {
                saw_removed = true;
            }
            _ => {}
        }
    }

    assert_eq!(cancellations.load(Ordering::SeqCst), 1);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn root_handle_can_add_and_remove_children_inside_nested_supervisor() {
    let (seed_tx, mut seed_rx) = mpsc::unbounded_channel();
    let (dynamic_tx, mut dynamic_rx) = mpsc::unbounded_channel();
    let dynamic_cancellations = Arc::new(AtomicUsize::new(0));

    let nested = SupervisorBuilder::new()
        .child(ChildSpec::new("seed", move |ctx| {
            let seed_tx = seed_tx.clone();
            async move {
                seed_tx.send(()).expect("test receiver dropped");
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }))
        .build()
        .expect("valid nested supervisor");

    let outer = SupervisorBuilder::new()
        .child(ChildSpec::new("anchor", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid outer supervisor");

    let handle = outer.spawn();
    let mut events = handle.subscribe();

    handle
        .add_child(nested.into_child_spec("nested"))
        .await
        .expect("dynamic nested child should be accepted");

    let mut saw_nested_supervisor_started = false;
    while !saw_nested_supervisor_started {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::SupervisorStarted) =>
            {
                saw_nested_supervisor_started = true;
            }
            _ => {}
        }
    }

    common::recv_event(&mut seed_rx).await;

    let dynamic_cancellations_for_child = dynamic_cancellations.clone();
    handle
        .add_child_at(
            ["nested"],
            ChildSpec::new("dynamic", move |ctx| {
                let dynamic_tx = dynamic_tx.clone();
                let dynamic_cancellations = dynamic_cancellations_for_child.clone();
                async move {
                    dynamic_tx.send(()).expect("test receiver dropped");
                    ctx.shutdown_token().cancelled().await;
                    dynamic_cancellations.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            }),
        )
        .await
        .expect("dynamic child inside nested supervisor should be accepted");

    common::recv_event(&mut dynamic_rx).await;

    handle
        .remove_child_at(["nested"], "dynamic")
        .await
        .expect("dynamic child inside nested supervisor should be removable");

    let mut saw_nested_dynamic_started = false;
    let mut saw_nested_remove_requested = false;
    let mut saw_nested_removed = false;

    while !(saw_nested_dynamic_started && saw_nested_remove_requested && saw_nested_removed) {
        match common::recv_supervisor_event(&mut events).await {
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::ChildStarted { ref id, generation: 0 } if id == "dynamic") =>
            {
                saw_nested_dynamic_started = true;
            }
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::ChildRemoveRequested { ref id } if id == "dynamic") =>
            {
                saw_nested_remove_requested = true;
            }
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::ChildRemoved { ref id } if id == "dynamic") =>
            {
                saw_nested_removed = true;
            }
            _ => {}
        }
    }

    assert_eq!(dynamic_cancellations.load(Ordering::SeqCst), 1);

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}

#[tokio::test]
async fn parent_event_stream_includes_forwarded_nested_events() {
    let nested = SupervisorBuilder::new()
        .child(ChildSpec::new("leaf", |_ctx| async move { Ok(()) }).restart(Restart::Temporary))
        .build()
        .expect("valid nested supervisor");

    let outer = SupervisorBuilder::new()
        .child(nested.into_child_spec("nested").restart(Restart::Temporary))
        .build()
        .expect("valid outer supervisor");

    let handle = outer.spawn();
    let mut events = handle.subscribe();
    let exit = handle
        .wait()
        .await
        .expect("outer supervisor should complete");
    assert!(matches!(exit, SupervisorExit::Completed));

    let mut saw_nested_supervisor_started = false;
    let mut saw_nested_leaf_started = false;
    let mut saw_nested_leaf_exit = false;

    loop {
        let event = common::recv_supervisor_event(&mut events).await;
        match event {
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::SupervisorStarted) =>
            {
                saw_nested_supervisor_started = true;
            }
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::ChildStarted { ref id, generation: 0 } if id == "leaf") =>
            {
                saw_nested_leaf_started = true;
            }
            SupervisorEvent::Nested {
                id,
                generation,
                event,
            } if id == "nested"
                && generation == 0
                && matches!(*event, SupervisorEvent::ChildExited { ref id, generation: 0, status: ExitStatusView::Completed } if id == "leaf") =>
            {
                saw_nested_leaf_exit = true;
            }
            SupervisorEvent::SupervisorStopped => break,
            _ => {}
        }
    }

    assert!(saw_nested_supervisor_started);
    assert!(saw_nested_leaf_started);
    assert!(saw_nested_leaf_exit);
}

#[tokio::test]
async fn nested_events_preserve_the_full_tree_path() {
    let deepest = SupervisorBuilder::new()
        .child(ChildSpec::new("leaf", |_ctx| async move { Ok(()) }).restart(Restart::Temporary))
        .build()
        .expect("valid deepest supervisor");

    let middle = SupervisorBuilder::new()
        .child(
            deepest
                .into_child_spec("middle")
                .restart(Restart::Temporary),
        )
        .build()
        .expect("valid middle supervisor");

    let outer = SupervisorBuilder::new()
        .child(middle.into_child_spec("outer").restart(Restart::Temporary))
        .build()
        .expect("valid outer supervisor");

    let handle = outer.spawn();
    let mut events = handle.subscribe();
    let exit = handle
        .wait()
        .await
        .expect("outer supervisor should complete");
    assert!(matches!(exit, SupervisorExit::Completed));

    loop {
        let event = common::recv_supervisor_event(&mut events).await;
        if let SupervisorEvent::Nested { .. } = &event
            && matches!(event.leaf(), SupervisorEvent::ChildStarted { id, generation: 0 } if id == "leaf")
        {
            assert_eq!(
                event.path(),
                vec![
                    EventPathSegment {
                        id: "outer".to_owned(),
                        generation: 0,
                    },
                    EventPathSegment {
                        id: "middle".to_owned(),
                        generation: 0,
                    },
                ]
            );
            break;
        }
    }
}

#[tokio::test]
async fn removing_nested_supervisor_unregisters_its_control_endpoint() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();

    let nested = SupervisorBuilder::new()
        .child(ChildSpec::new("leaf", move |ctx| {
            let started_tx = started_tx.clone();
            async move {
                started_tx.send(()).expect("test receiver dropped");
                ctx.shutdown_token().cancelled().await;
                Ok(())
            }
        }))
        .build()
        .expect("valid nested supervisor");

    let handle = SupervisorBuilder::new()
        .child(ChildSpec::new("anchor", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid outer supervisor")
        .spawn();

    handle
        .add_child(nested.into_child_spec("nested"))
        .await
        .expect("nested child should be accepted");
    common::recv_event(&mut started_rx).await;

    handle
        .remove_child("nested")
        .await
        .expect("nested child should be removable");

    let add_err = handle
        .add_child_at(
            ["nested"],
            ChildSpec::new("late", |_ctx| async move { Ok(()) }),
        )
        .await
        .expect_err("removed nested supervisor should no longer accept child commands");
    assert_eq!(add_err, ControlError::UnknownChildId("nested".to_owned()));

    let remove_err = handle
        .remove_child_at(["nested"], "late")
        .await
        .expect_err("removed nested supervisor should no longer accept remove commands");
    assert_eq!(
        remove_err,
        ControlError::UnknownChildId("nested".to_owned())
    );

    handle.shutdown();
    let exit = handle.wait().await.expect("shutdown should succeed");
    assert!(matches!(exit, SupervisorExit::Shutdown));
}
