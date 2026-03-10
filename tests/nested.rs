use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::{
    sync::{broadcast, mpsc},
    time::timeout,
};
use tokio_supervisor::{
    ChildSpec, EventPathSegment, ExitStatusView, Restart, SupervisorBuilder, SupervisorEvent,
    SupervisorExit,
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
                        ctx.token.cancelled().await;
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
                ctx.token.cancelled().await;
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
        let event = recv_supervisor_event(&mut events).await;
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
        let event = recv_supervisor_event(&mut events).await;
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

async fn recv_supervisor_event(
    events: &mut broadcast::Receiver<SupervisorEvent>,
) -> SupervisorEvent {
    match timeout(common::EVENT_TIMEOUT, events.recv())
        .await
        .expect("timed out waiting for supervisor event")
    {
        Ok(event) => event,
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            panic!("lagged while reading supervisor events: skipped {skipped}");
        }
        Err(broadcast::error::RecvError::Closed) => {
            panic!("supervisor event stream closed unexpectedly");
        }
    }
}
