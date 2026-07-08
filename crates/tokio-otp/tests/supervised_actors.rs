use std::{
    io,
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_actor::{Actor, ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder};
use tokio_otp::{BuildError, SupervisedActors};
use tokio_supervisor::{Restart, Strategy, SupervisorBuilder};

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

impl Actor for Frontend {
    type Msg = String;

    async fn run(&self, mut ctx: ActorContext<String>) -> ActorResult {
        self.starts.fetch_add(1, Ordering::SeqCst);
        while let Some(message) = ctx.recv().await {
            let mut worker = self.worker.clone();
            worker.send_when_ready(message).await?;
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

impl Actor for Worker {
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
    let worker_ref = builder.declare::<String>("worker");
    let mut frontend_ref = builder.actor(
        "frontend",
        Frontend {
            worker: worker_ref,
            starts: Arc::clone(&frontend_starts),
        },
    );
    builder.actor(
        "worker",
        Worker {
            observed: observed_tx,
            starts: Arc::clone(&worker_starts),
            failed: oneshot_slot(failed_tx),
        },
    );
    let graph = builder.build().expect("valid graph");

    let supervisor = SupervisedActors::new(graph)
        .expect("graph decomposes")
        .restart(Restart::Transient)
        .build_supervisor(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("supervisor builds");

    let handle = supervisor.spawn();

    frontend_ref.wait_for_binding().await;
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

struct Drain<M>(PhantomData<fn(M)>);

impl<M> Drain<M> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<M> Clone for Drain<M> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<M: Send + 'static> Actor for Drain<M> {
    type Msg = M;

    async fn run(&self, mut ctx: ActorContext<M>) -> ActorResult {
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[test]
fn supervised_actors_reject_unknown_actor_overrides() {
    let mut builder = GraphBuilder::new();
    builder.actor("worker", Drain::<()>::new());
    let graph = builder.build().expect("valid graph");

    let result = SupervisedActors::new(graph)
        .expect("graph decomposes")
        .actor_restart("missing", Restart::Permanent)
        .build();

    assert!(matches!(
        result,
        Err(BuildError::UnknownActor { actor_id }) if actor_id == "missing"
    ));
}
