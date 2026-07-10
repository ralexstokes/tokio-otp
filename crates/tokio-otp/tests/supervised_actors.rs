use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    time::{sleep, timeout},
};
use tokio_otp::{
    ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder, RawActor, Reply, SendError,
    SupervisedActors,
};
use tokio_supervisor::{
    BackoffPolicy, ChildStateView, ExitStatusView, Restart, RestartIntensity, Strategy,
    SupervisorBuilder,
};

fn oneshot_slot<T>(tx: oneshot::Sender<T>) -> Arc<Mutex<Option<oneshot::Sender<T>>>> {
    Arc::new(Mutex::new(Some(tx)))
}

fn send_once<T>(slot: &Arc<Mutex<Option<oneshot::Sender<T>>>>, value: T) {
    if let Some(tx) = slot.lock().expect("mutex not poisoned").take() {
        let _ = tx.send(value);
    }
}

#[derive(Clone)]
struct Frontend {
    worker: ActorRef<String>,
    starts: Arc<AtomicUsize>,
}

impl RawActor for Frontend {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        self.starts.fetch_add(1, Ordering::SeqCst);
        while let Some(message) = ctx.recv().await {
            let worker = self.worker.clone();
            worker.send(message).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Worker {
    observed: mpsc::UnboundedSender<String>,
    starts: Arc<AtomicUsize>,
    failed: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl RawActor for Worker {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let run = self.starts.fetch_add(1, Ordering::SeqCst);
        while let Some(message) = ctx.recv().await {
            self.observed.send(message).expect("receiver alive");
            if run == 0 {
                send_once(&self.failed, ());
                return Err::<(), BoxError>(Box::new(io::Error::other("boom")));
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn supervised_actors_restart_only_the_failed_actor() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let frontend_starts = Arc::new(AtomicUsize::new(0));
    let worker_starts = Arc::new(AtomicUsize::new(0));
    let (failed_tx, failed_rx) = oneshot::channel();

    let mut builder = GraphBuilder::new();
    let (worker_slot, worker_ref) = builder.slot::<String>("worker");
    let frontend_ref = builder.actor(
        "frontend",
        Frontend {
            worker: worker_ref,
            starts: Arc::clone(&frontend_starts),
        },
    );
    builder.define(
        worker_slot,
        Worker {
            observed: observed_tx,
            starts: Arc::clone(&worker_starts),
            failed: oneshot_slot(failed_tx),
        },
    );
    let graph = builder.build().expect("valid graph");

    let supervisor = SupervisedActors::new(graph)
        .restart(Restart::Transient)
        .build_supervisor(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("supervisor builds");

    let handle = supervisor.spawn();

    frontend_ref
        .send("first".to_owned())
        .await
        .expect("frontend accepts the first message");
    let first = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker saw the first message")
        .expect("worker forwarded the first message");
    assert_eq!(first, "first");

    timeout(Duration::from_secs(1), failed_rx)
        .await
        .expect("worker failed on the first run")
        .expect("worker failure signal received");

    frontend_ref
        .send("second".to_owned())
        .await
        .expect("frontend accepts the second message");
    let second = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("worker saw the second message after restart")
        .expect("worker forwarded the second message");
    assert_eq!(second, "second");

    assert_eq!(frontend_starts.load(Ordering::SeqCst), 1);
    assert!(worker_starts.load(Ordering::SeqCst) >= 2);

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}

#[derive(Clone)]
struct CleanThenReceive {
    runs: Arc<AtomicUsize>,
    first_exited: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    observed: mpsc::UnboundedSender<String>,
}

impl RawActor for CleanThenReceive {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        if run == 0 {
            send_once(&self.first_exited, ());
            return Ok(());
        }

        while let Some(message) = ctx.recv().await {
            self.observed.send(message).expect("receiver alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn send_waits_during_permanent_restart_window() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (first_exited_tx, first_exited_rx) = oneshot::channel();
    let runs = Arc::new(AtomicUsize::new(0));

    let mut builder = GraphBuilder::new();
    let worker_ref = builder.actor(
        "worker",
        CleanThenReceive {
            runs,
            first_exited: oneshot_slot(first_exited_tx),
            observed: observed_tx,
        },
    );
    let graph = builder.build().expect("valid graph");

    let supervisor = SupervisedActors::new(graph)
        .actor_restart(&worker_ref, Restart::Permanent)
        .actor_restart_intensity(
            &worker_ref,
            RestartIntensity::new(10, Duration::from_secs(1))
                .with_backoff(BackoffPolicy::Fixed(Duration::from_millis(100))),
        )
        .build_supervisor(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("supervisor builds");
    let handle = supervisor.spawn();

    timeout(Duration::from_secs(1), first_exited_rx)
        .await
        .expect("first run exited")
        .expect("first run signal received");

    let send_task = tokio::spawn({
        let worker_ref = worker_ref.clone();
        async move { worker_ref.send("after-rebind".to_owned()).await }
    });
    sleep(Duration::from_millis(25)).await;
    assert!(
        !send_task.is_finished(),
        "send should wait during the restart backoff"
    );

    let observed = timeout(Duration::from_secs(1), observed_rx.recv())
        .await
        .expect("message delivered after restart")
        .expect("message observed");
    assert_eq!(observed, "after-rebind");
    send_task
        .await
        .expect("send task joined")
        .expect("send completed");

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}

#[derive(Clone)]
struct NotifyCleanExit {
    exited: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl RawActor for NotifyCleanExit {
    type Msg = ();

    async fn run(&self, _ctx: ActorContext<()>) -> ActorResult {
        send_once(&self.exited, ());
        Ok(())
    }
}

#[tokio::test]
async fn send_to_cleanly_exiting_transient_returns_actor_terminated_promptly() {
    let (exited_tx, exited_rx) = oneshot::channel();

    let mut builder = GraphBuilder::new();
    let worker_ref = builder.actor(
        "worker",
        NotifyCleanExit {
            exited: oneshot_slot(exited_tx),
        },
    );
    let graph = builder.build().expect("valid graph");

    let supervisor = SupervisedActors::new(graph)
        .restart(Restart::Transient)
        .build_supervisor(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("supervisor builds");
    let handle = supervisor.spawn();

    timeout(Duration::from_secs(1), exited_rx)
        .await
        .expect("actor exited")
        .expect("exit signal received");
    let result = timeout(Duration::from_millis(100), worker_ref.send(()))
        .await
        .expect("send returned promptly");
    assert!(matches!(
        result,
        Err(SendError::ActorTerminated { actor_id }) if actor_id == "worker"
    ));

    let mut snapshots = handle.subscribe_snapshots();
    let completed = timeout(
        Duration::from_secs(1),
        snapshots.wait_for(|snapshot| {
            snapshot
                .child("worker")
                .is_some_and(|child| child.state == ChildStateView::Stopped)
        }),
    )
    .await
    .expect("actor should complete")
    .expect("snapshot stream should remain open")
    .clone();
    assert!(matches!(
        completed
            .child("worker")
            .expect("worker remains visible")
            .last_exit
            .as_ref(),
        Some(ExitStatusView::Completed)
    ));

    handle.shutdown();
    handle.wait().await.expect("shutdown should succeed");
}

enum RpcMsg {
    FailOnce,
    Get(Reply<String>),
}

#[derive(Clone)]
struct RestartingRpc {
    runs: Arc<AtomicUsize>,
    failed: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl RawActor for RestartingRpc {
    type Msg = RpcMsg;

    async fn run(&self, mut ctx: ActorContext<RpcMsg>) -> ActorResult {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        while let Some(message) = ctx.recv().await {
            match message {
                RpcMsg::FailOnce if run == 0 => {
                    send_once(&self.failed, ());
                    return Err::<(), BoxError>(Box::new(io::Error::other("boom")));
                }
                RpcMsg::FailOnce => {}
                RpcMsg::Get(reply) => reply.send("ok".to_owned()),
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn call_succeeds_across_restart_window() {
    let (failed_tx, failed_rx) = oneshot::channel();
    let runs = Arc::new(AtomicUsize::new(0));

    let mut builder = GraphBuilder::new();
    let rpc_ref = builder.actor(
        "rpc",
        RestartingRpc {
            runs,
            failed: oneshot_slot(failed_tx),
        },
    );
    let graph = builder.build().expect("valid graph");

    let supervisor = SupervisedActors::new(graph)
        .actor_restart(&rpc_ref, Restart::Transient)
        .actor_restart_intensity(
            &rpc_ref,
            RestartIntensity::new(10, Duration::from_secs(1))
                .with_backoff(BackoffPolicy::Fixed(Duration::from_millis(100))),
        )
        .build_supervisor(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("supervisor builds");
    let handle = supervisor.spawn();

    rpc_ref
        .send(RpcMsg::FailOnce)
        .await
        .expect("first request delivered");
    timeout(Duration::from_secs(1), failed_rx)
        .await
        .expect("actor failed")
        .expect("failure signal received");

    let call_task = tokio::spawn({
        let rpc_ref = rpc_ref.clone();
        async move { rpc_ref.call(RpcMsg::Get).await }
    });
    sleep(Duration::from_millis(25)).await;
    assert!(
        !call_task.is_finished(),
        "call should wait during the restart backoff"
    );

    assert_eq!(
        call_task
            .await
            .expect("call task joined")
            .expect("call completed after restart"),
        "ok"
    );

    handle
        .shutdown_and_wait()
        .await
        .expect("supervisor shut down cleanly");
}
