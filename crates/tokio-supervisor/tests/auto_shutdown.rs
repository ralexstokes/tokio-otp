use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_supervisor::{
    AutoShutdown, ChildSpec, ExitStatusView, RestartPolicy, SupervisorBuilder, SupervisorEvent,
    SupervisorSpec,
};

mod common;

#[tokio::test]
async fn any_significant_clean_exit_stops_siblings_and_supervisor() {
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let supervisor = SupervisorBuilder::new()
        .auto_shutdown(AutoShutdown::AnySignificant)
        .child(ChildSpec::new("trigger", |_| async { Ok(()) }).significant())
        .child(ChildSpec::new("sibling", move |ctx| {
            let cancelled_tx = cancelled_tx.clone();
            async move {
                ctx.shutdown_token().cancelled().await;
                cancelled_tx.send(()).expect("test receiver dropped");
                Ok(())
            }
        }))
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    let mut events = handle.subscribe();
    handle.wait().await.expect("auto-shutdown should be clean");
    common::recv_event(&mut cancelled_rx).await;

    let mut saw_trigger = false;
    while let Ok(event) = events.try_recv() {
        if event
            == (SupervisorEvent::AutoShutdownTriggered {
                id: "trigger".to_owned(),
                mode: AutoShutdown::AnySignificant,
            })
        {
            saw_trigger = true;
        }
    }
    assert!(
        saw_trigger,
        "auto-shutdown event should identify its trigger"
    );
}

#[tokio::test]
async fn all_significant_waits_for_last_clean_exit() {
    let (first_tx, first_rx) = oneshot::channel::<()>();
    let first_rx = Arc::new(std::sync::Mutex::new(Some(first_rx)));
    let (second_tx, second_rx) = oneshot::channel::<()>();
    let second_rx = Arc::new(std::sync::Mutex::new(Some(second_rx)));

    let supervisor = SupervisorBuilder::new()
        .auto_shutdown(AutoShutdown::AllSignificant)
        .child(
            ChildSpec::new("first", move |_| {
                let rx = first_rx.lock().expect("lock poisoned").take().unwrap();
                async move {
                    rx.await.expect("gate dropped");
                    Ok(())
                }
            })
            .restart(RestartPolicy::Never)
            .significant(),
        )
        .child(
            ChildSpec::new("second", move |_| {
                let rx = second_rx.lock().expect("lock poisoned").take().unwrap();
                async move {
                    rx.await.expect("gate dropped");
                    Ok(())
                }
            })
            .restart(RestartPolicy::Never)
            .significant(),
        )
        .build()
        .expect("valid supervisor");

    let handle = supervisor.spawn();
    first_tx.send(()).expect("first child dropped");
    assert!(
        timeout(common::QUIET_TIMEOUT, handle.wait()).await.is_err(),
        "one significant child must not be enough"
    );
    second_tx.send(()).expect("second child dropped");
    handle
        .wait()
        .await
        .expect("last significant exit should stop cleanly");
}

#[tokio::test]
async fn significant_failure_restarts_before_clean_exit_triggers_shutdown() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_child = attempts.clone();
    let supervisor = SupervisorBuilder::new()
        .auto_shutdown(AutoShutdown::AnySignificant)
        .child(
            ChildSpec::new("trigger", move |_| {
                let attempt = attempts_for_child.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        Err("first attempt fails".into())
                    } else {
                        Ok(())
                    }
                }
            })
            .significant(),
        )
        .build()
        .expect("valid supervisor");

    supervisor
        .spawn()
        .wait()
        .await
        .expect("second clean exit should stop supervisor");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn nested_auto_shutdown_is_a_clean_child_exit_to_parent() {
    let inner = SupervisorBuilder::new()
        .auto_shutdown(AutoShutdown::AnySignificant)
        .child(ChildSpec::new("done", |_| async { Ok(()) }).significant())
        .build()
        .expect("valid inner supervisor");
    let parent = SupervisorBuilder::new()
        .supervisor(
            "job",
            SupervisorSpec::new(inner).restart(RestartPolicy::OnFailure),
        )
        .build()
        .expect("valid parent supervisor");

    let handle = parent.spawn();
    let mut events = handle.subscribe();
    let event = loop {
        let event = common::recv_supervisor_event(&mut events).await;
        if matches!(
            &event,
            SupervisorEvent::ChildExited {
                id,
                status: ExitStatusView::Completed,
                ..
            } if id == "job"
        ) {
            break event;
        }
    };
    assert!(matches!(event, SupervisorEvent::ChildExited { .. }));
    assert!(
        timeout(common::QUIET_TIMEOUT, handle.wait()).await.is_err(),
        "parent should continue after clean child exit"
    );
    handle
        .shutdown_and_wait()
        .await
        .expect("parent shutdown succeeds");
}
