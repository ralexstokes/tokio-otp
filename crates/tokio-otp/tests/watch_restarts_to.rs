use std::{io, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_otp::{
    Actor, ActorContext, ActorRef, ActorResult, DynamicActorOptions, RestartPolicy, Runtime,
    RuntimeHandle, SupervisorHandleExt,
};

struct Sink {
    observed: mpsc::UnboundedSender<u64>,
}

impl Actor for Sink {
    type Msg = u64;

    async fn handle(&mut self, count: u64, _ctx: &ActorContext<u64>) -> ActorResult {
        if count == 0 {
            return Err(io::Error::other("sink crash requested").into());
        }
        self.observed.send(count).expect("observer alive");
        Ok(())
    }
}

struct Crasher;

impl Actor for Crasher {
    type Msg = ();

    async fn handle(&mut self, (): (), _ctx: &ActorContext<()>) -> ActorResult {
        Err(io::Error::other("crash requested").into())
    }
}

async fn runtime_with_sink() -> (
    RuntimeHandle,
    ActorRef<u64>,
    ActorRef<()>,
    mpsc::UnboundedReceiver<u64>,
) {
    let runtime = Runtime::builder().build().expect("runtime builds");
    let handle = runtime.spawn();
    let (observed_tx, observed_rx) = mpsc::unbounded_channel();
    let sink = handle
        .add_actor(
            "sink",
            move || Sink {
                observed: observed_tx.clone(),
            },
            DynamicActorOptions::new().restart(RestartPolicy::OnFailure),
        )
        .await
        .expect("sink added");
    let crasher = handle
        .add_actor(
            "crasher",
            || Crasher,
            DynamicActorOptions::new().restart(RestartPolicy::OnFailure),
        )
        .await
        .expect("crasher added");
    handle.wait_started().await.expect("actors started");

    (handle, sink, crasher, observed_rx)
}

async fn crash_once(handle: &RuntimeHandle, crasher: &ActorRef<()>) {
    let restarted = handle
        .monitor_restart("crasher")
        .expect("crasher is a direct child");
    crasher.send(()).await.expect("crash request delivered");
    timeout(Duration::from_secs(1), restarted)
        .await
        .expect("crasher restarted in time")
        .expect("crasher restarted");
}

#[tokio::test]
async fn runtime_watch_restarts_to_forwards_counts() {
    let (handle, sink, crasher, mut observed) = runtime_with_sink().await;
    let watch = handle.watch_restarts_to(&sink, |count| count);

    crash_once(&handle, &crasher).await;
    assert_eq!(
        timeout(Duration::from_secs(1), observed.recv())
            .await
            .expect("restart count delivered"),
        Some(1)
    );

    assert!(!watch.is_cancelled());
    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn dropping_restart_watch_stops_delivery() {
    let (handle, sink, crasher, mut observed) = runtime_with_sink().await;
    let watch = handle.watch_restarts_to(&sink, |count| count);
    drop(watch);

    crash_once(&handle, &crasher).await;
    assert!(
        timeout(Duration::from_millis(100), observed.recv())
            .await
            .is_err(),
        "dropped restart watch delivered a message"
    );

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn restart_watch_follows_target_restarts() {
    let (handle, sink, _crasher, mut observed) = runtime_with_sink().await;
    let watch = handle.watch_restarts_to(&sink, |count| count);
    let restarted = handle
        .monitor_restart("sink")
        .expect("sink is a direct child");

    sink.send(0).await.expect("sink crash request delivered");
    timeout(Duration::from_secs(1), restarted)
        .await
        .expect("sink restarted in time")
        .expect("sink restarted");
    assert_eq!(
        timeout(Duration::from_secs(1), observed.recv())
            .await
            .expect("restart count delivered to new incarnation"),
        Some(1)
    );
    assert!(!watch.is_cancelled());

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn restart_watch_stops_when_target_terminates() {
    let (handle, sink, _crasher, _observed) = runtime_with_sink().await;
    let watch = handle
        .supervisor_handle()
        .watch_restarts_to(&sink, |count| count);

    handle.remove_child("sink").await.expect("sink removed");
    timeout(Duration::from_secs(1), async {
        while !watch.is_cancelled() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("watch stopped after target termination");

    handle.shutdown_and_wait().await.expect("clean shutdown");
}
