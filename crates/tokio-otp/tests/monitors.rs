use std::{future::pending, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tokio_otp::{
    ActorContext, ActorRef, ActorResult, Down, DownReason, RawActor, RebindPolicy, RunnableActor,
    RunnableActorFactory,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum PeerMessage {
    Stop,
    Panic,
}

#[derive(Clone)]
struct Peer {
    started: mpsc::UnboundedSender<()>,
}

impl RawActor for Peer {
    type Msg = PeerMessage;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        self.started.send(()).expect("start receiver alive");
        match ctx.recv().await {
            Some(PeerMessage::Stop) | None => Ok(()),
            Some(PeerMessage::Panic) => panic!("deliberate peer panic"),
        }
    }
}

enum ObserverMessage {
    Down(Down),
    Watch,
    Crash,
}

#[derive(Clone)]
struct Observer {
    peer: ActorRef<PeerMessage>,
    observed: mpsc::UnboundedSender<Down>,
    started: mpsc::UnboundedSender<()>,
    cancel_monitor: bool,
}

impl RawActor for Observer {
    type Msg = ObserverMessage;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        let monitor = ctx.monitor(&self.peer, ObserverMessage::Down);
        if self.cancel_monitor {
            monitor.cancel();
        }
        self.started.send(()).expect("start receiver alive");

        while let Some(message) = ctx.recv().await {
            match message {
                ObserverMessage::Down(down) => {
                    self.observed.send(down).expect("observer receiver alive");
                }
                ObserverMessage::Watch => {
                    ctx.monitor(&self.peer, ObserverMessage::Down);
                }
                ObserverMessage::Crash => panic!("deliberate observer panic"),
            }
        }
        Ok(())
    }
}

struct Fixture {
    peer: RunnableActor,
    peer_ref: ActorRef<PeerMessage>,
    peer_started: mpsc::UnboundedReceiver<()>,
    observer: RunnableActor,
    observer_ref: ActorRef<ObserverMessage>,
    observer_started: mpsc::UnboundedReceiver<()>,
    observed: mpsc::UnboundedReceiver<Down>,
}

fn fixture(cancel_monitor: bool) -> Fixture {
    let factory = RunnableActorFactory::new();
    let (peer_started_tx, peer_started) = mpsc::unbounded_channel();
    let (peer, peer_ref) = factory.actor(
        "peer",
        Peer {
            started: peer_started_tx,
        },
    );
    let (observed_tx, observed) = mpsc::unbounded_channel();
    let (observer_started_tx, observer_started) = mpsc::unbounded_channel();
    let (observer, observer_ref) = factory.actor(
        "observer",
        Observer {
            peer: peer_ref.clone(),
            observed: observed_tx,
            started: observer_started_tx,
            cancel_monitor,
        },
    );
    Fixture {
        peer,
        peer_ref,
        peer_started,
        observer,
        observer_ref,
        observer_started,
        observed,
    }
}

async fn started(receiver: &mut mpsc::UnboundedReceiver<()>) {
    timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("actor started promptly")
        .expect("start sender alive");
}

async fn down(receiver: &mut mpsc::UnboundedReceiver<Down>) -> Down {
    timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("down delivered promptly")
        .expect("observer sender alive")
}

#[tokio::test]
async fn monitor_reports_panicked_peer_as_failure() {
    let mut fixture = fixture(false);
    let peer = fixture.peer.clone();
    let peer_task =
        tokio::spawn(async move { peer.run_until(pending::<()>(), RebindPolicy::Never).await });
    let observer_stop = CancellationToken::new();
    let observer = fixture.observer.clone();
    let stop = observer_stop.clone();
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(stop.cancelled(), RebindPolicy::Never)
            .await
    });
    started(&mut fixture.peer_started).await;
    started(&mut fixture.observer_started).await;

    fixture
        .peer_ref
        .send(PeerMessage::Panic)
        .await
        .expect("panic command sent");
    let notification = down(&mut fixture.observed).await;
    assert_eq!(notification.actor_id, "peer");
    assert_eq!(notification.generation, 0);
    assert_eq!(notification.reason, DownReason::Failure);

    assert!(peer_task.await.expect_err("peer task panicked").is_panic());
    observer_stop.cancel();
    observer_task
        .await
        .expect("observer task joined")
        .expect("observer stopped cleanly");
}

#[tokio::test]
async fn monitor_reports_clean_stop_as_normal() {
    let mut fixture = fixture(false);
    let peer = fixture.peer.clone();
    let peer_task =
        tokio::spawn(async move { peer.run_until(pending::<()>(), RebindPolicy::Never).await });
    let observer_stop = CancellationToken::new();
    let stop = observer_stop.clone();
    let observer = fixture.observer.clone();
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(stop.cancelled(), RebindPolicy::Never)
            .await
    });
    started(&mut fixture.peer_started).await;
    started(&mut fixture.observer_started).await;

    fixture
        .peer_ref
        .send(PeerMessage::Stop)
        .await
        .expect("stop command sent");
    assert_eq!(down(&mut fixture.observed).await.reason, DownReason::Normal);
    peer_task
        .await
        .expect("peer task joined")
        .expect("peer stopped cleanly");

    observer_stop.cancel();
    observer_task
        .await
        .expect("observer task joined")
        .expect("observer stopped cleanly");
}

#[tokio::test]
async fn cancelled_monitor_suppresses_delivery() {
    let mut fixture = fixture(true);
    let peer = fixture.peer.clone();
    let peer_task =
        tokio::spawn(async move { peer.run_until(pending::<()>(), RebindPolicy::Never).await });
    let observer = fixture.observer.clone();
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut fixture.peer_started).await;
    started(&mut fixture.observer_started).await;

    fixture
        .peer_ref
        .send(PeerMessage::Panic)
        .await
        .expect("panic command sent");
    assert!(
        timeout(Duration::from_millis(100), fixture.observed.recv())
            .await
            .is_err()
    );
    assert!(peer_task.await.expect_err("peer task panicked").is_panic());
    observer_task.abort();
}

#[tokio::test]
async fn observer_restart_clears_old_monitors() {
    let mut fixture = fixture(false);
    let peer = fixture.peer.clone();
    let peer_task =
        tokio::spawn(async move { peer.run_until(pending::<()>(), RebindPolicy::Never).await });
    started(&mut fixture.peer_started).await;

    let first_observer = fixture.observer.clone();
    let first_task = tokio::spawn(async move {
        first_observer
            .run_until(pending::<()>(), RebindPolicy::OnFailure)
            .await
    });
    started(&mut fixture.observer_started).await;
    fixture
        .observer_ref
        .send(ObserverMessage::Crash)
        .await
        .expect("crash command sent");
    assert!(
        first_task
            .await
            .expect_err("observer task panicked")
            .is_panic()
    );

    let second_observer = fixture.observer.clone();
    let second_task = tokio::spawn(async move {
        second_observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut fixture.observer_started).await;
    fixture
        .peer_ref
        .send(PeerMessage::Stop)
        .await
        .expect("stop command sent");
    assert_eq!(down(&mut fixture.observed).await.reason, DownReason::Normal);
    assert!(
        timeout(Duration::from_millis(100), fixture.observed.recv())
            .await
            .is_err(),
        "the cancelled first-incarnation monitor must not also deliver"
    );

    peer_task
        .await
        .expect("peer task joined")
        .expect("peer stopped cleanly");
    second_task.abort();
}

#[tokio::test]
async fn monitoring_dead_peer_delivers_immediate_no_process() {
    let mut fixture = fixture(false);
    fixture.peer.terminate_binding();
    let observer = fixture.observer.clone();
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut fixture.observer_started).await;

    let notification = down(&mut fixture.observed).await;
    assert_eq!(notification.actor_id, "peer");
    assert_eq!(notification.generation, 0);
    assert_eq!(notification.reason, DownReason::NoProcess);
    observer_task.abort();
}

#[tokio::test]
async fn monitoring_detached_peer_delivers_immediate_no_process() {
    let factory = RunnableActorFactory::new();
    let (observed_tx, mut observed) = mpsc::unbounded_channel();
    let (started_tx, mut observer_started) = mpsc::unbounded_channel();
    let detached_peer = {
        let (_, peer_ref) = factory.actor(
            "detached-peer",
            Peer {
                started: mpsc::unbounded_channel().0,
            },
        );
        peer_ref
    };
    let (observer, _) = factory.actor(
        "observer",
        Observer {
            peer: detached_peer,
            observed: observed_tx,
            started: started_tx,
            cancel_monitor: false,
        },
    );
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut observer_started).await;

    assert_eq!(down(&mut observed).await.reason, DownReason::NoProcess);
    observer_task.abort();
}

#[tokio::test]
async fn monitor_generation_increments_after_peer_restart() {
    let mut fixture = fixture(false);
    let observer = fixture.observer.clone();
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    let first_peer = fixture.peer.clone();
    let first_task = tokio::spawn(async move {
        first_peer
            .run_until(pending::<()>(), RebindPolicy::OnFailure)
            .await
    });
    started(&mut fixture.peer_started).await;
    started(&mut fixture.observer_started).await;
    fixture
        .peer_ref
        .send(PeerMessage::Panic)
        .await
        .expect("panic command sent");
    assert_eq!(down(&mut fixture.observed).await.generation, 0);
    assert!(
        first_task
            .await
            .expect_err("first peer task panicked")
            .is_panic()
    );

    let second_peer = fixture.peer.clone();
    let second_task = tokio::spawn(async move {
        second_peer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut fixture.peer_started).await;
    fixture
        .observer_ref
        .send(ObserverMessage::Watch)
        .await
        .expect("watch command sent");
    fixture
        .peer_ref
        .send(PeerMessage::Stop)
        .await
        .expect("stop command sent");
    let notification = down(&mut fixture.observed).await;
    assert_eq!(notification.generation, 1);
    assert_eq!(notification.reason, DownReason::Normal);

    second_task
        .await
        .expect("second peer task joined")
        .expect("second peer stopped cleanly");
    observer_task.abort();
}
