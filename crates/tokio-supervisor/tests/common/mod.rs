#![allow(dead_code)]

use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, broadcast, mpsc, watch},
    time::timeout,
};
use tokio_supervisor::{
    BoxError, ChildSnapshot, ChildSpec, ChildStateView, RestartIntensity, RestartPolicy,
    SupervisorEvent, SupervisorHandle, SupervisorSnapshot,
};

pub const EVENT_TIMEOUT: Duration = Duration::from_secs(2);
pub const QUIET_TIMEOUT: Duration = Duration::from_millis(150);
pub const SHORT_GRACE: Duration = Duration::from_millis(50);

pub fn test_error(message: &'static str) -> BoxError {
    Box::new(std::io::Error::other(message))
}

pub async fn recv_event<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T
where
    T: Debug,
{
    timeout(EVENT_TIMEOUT, rx.recv())
        .await
        .expect("timed out waiting for channel event")
        .expect("channel closed before expected event arrived")
}

pub async fn recv_n<T>(rx: &mut mpsc::UnboundedReceiver<T>, n: usize) -> Vec<T>
where
    T: Debug,
{
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        items.push(recv_event(rx).await);
    }
    items
}

pub async fn assert_no_event<T>(rx: &mut mpsc::UnboundedReceiver<T>)
where
    T: Debug,
{
    if let Ok(Some(value)) = timeout(QUIET_TIMEOUT, rx.recv()).await {
        panic!("unexpected event arrived: {value:?}");
    }
}

#[derive(Clone, Debug)]
pub struct LiveFlag(Arc<AtomicBool>);

impl LiveFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn is_live(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn guard(&self) -> LiveGuard {
        self.0.store(true, Ordering::SeqCst);
        LiveGuard(self.0.clone())
    }
}

pub struct LiveGuard(Arc<AtomicBool>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub async fn recv_supervisor_event(
    events: &mut broadcast::Receiver<SupervisorEvent>,
) -> SupervisorEvent {
    match timeout(EVENT_TIMEOUT, events.recv())
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

pub fn fail_on_generations(
    id: &'static str,
    trigger_failure: Arc<Notify>,
    generations_to_fail: u64,
) -> ChildSpec {
    ChildSpec::new(id, move |ctx| {
        let trigger_failure = trigger_failure.clone();
        async move {
            if ctx.generation() < generations_to_fail {
                trigger_failure.notified().await;
                return Err(test_error("boom"));
            }

            ctx.shutdown_token().cancelled().await;
            Ok(())
        }
    })
    .restart(RestartPolicy::OnFailure)
}

pub fn failing_child(
    id: &'static str,
    trigger_failure: &Arc<Notify>,
    error: &'static str,
) -> ChildSpec {
    let trigger_failure = trigger_failure.clone();
    ChildSpec::new(id, move |_ctx| {
        let trigger_failure = trigger_failure.clone();
        async move {
            trigger_failure.notified().await;
            Err(test_error(error))
        }
    })
    .restart(RestartPolicy::OnFailure)
    .restart_intensity(RestartIntensity::new(0, Duration::from_secs(60)))
}

pub async fn wait_for_child_running(
    snapshots: &mut watch::Receiver<SupervisorSnapshot>,
    id: &str,
    generation: u64,
) -> ChildSnapshot {
    wait_for_snapshot(snapshots, |snapshot| {
        snapshot.child(id).is_some_and(|child| {
            child.generation == generation && child.state == ChildStateView::Running
        })
    })
    .await
    .child(id)
    .expect("child should exist in matching snapshot")
    .clone()
}

pub async fn wait_for_snapshot(
    snapshots: &mut watch::Receiver<SupervisorSnapshot>,
    predicate: impl Fn(&SupervisorSnapshot) -> bool,
) -> SupervisorSnapshot {
    if predicate(&snapshots.borrow()) {
        return snapshots.borrow().clone();
    }

    loop {
        timeout(EVENT_TIMEOUT, snapshots.changed())
            .await
            .expect("timed out waiting for snapshot update")
            .expect("snapshot stream closed unexpectedly");

        let snapshot = snapshots.borrow().clone();
        if predicate(&snapshot) {
            return snapshot;
        }
    }
}

pub async fn shutdown(handle: SupervisorHandle) {
    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}
