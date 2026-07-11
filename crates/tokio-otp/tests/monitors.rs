use std::{future::pending, sync::Arc, time::Duration};

use tokio::{
    sync::{Notify, mpsc},
    time::timeout,
};
use tokio_otp::{
    ActorContext, ActorRef, ActorResult, Down, DownReason, GraphBuilder, MonitorRef, RawActor,
    RebindPolicy, RunnableActor, RunnableActorFactory, SupervisedActors,
};
use tokio_supervisor::{ShutdownPolicy, SupervisorBuilder};
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

#[tokio::test]
async fn pre_start_monitor_attaches_to_first_incarnation() {
    let mut fixture = fixture(false);
    let observer = fixture.observer.clone();
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut fixture.observer_started).await;
    assert!(
        timeout(Duration::from_millis(50), fixture.observed.recv())
            .await
            .is_err()
    );

    let peer = fixture.peer.clone();
    let peer_task =
        tokio::spawn(async move { peer.run_until(pending::<()>(), RebindPolicy::Never).await });
    started(&mut fixture.peer_started).await;
    fixture
        .peer_ref
        .send(PeerMessage::Stop)
        .await
        .expect("stop command sent");
    let notification = down(&mut fixture.observed).await;
    assert_eq!(notification.generation, 0);
    assert_eq!(notification.reason, DownReason::Normal);

    peer_task
        .await
        .expect("peer task joined")
        .expect("peer stopped cleanly");
    observer_task.abort();
}

#[tokio::test]
async fn shutdown_request_reports_normal_exit() {
    let mut fixture = fixture(false);
    let peer_stop = CancellationToken::new();
    let stop = peer_stop.clone();
    let peer = fixture.peer.clone();
    let peer_task =
        tokio::spawn(async move { peer.run_until(stop.cancelled(), RebindPolicy::Never).await });
    let observer = fixture.observer.clone();
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut fixture.peer_started).await;
    started(&mut fixture.observer_started).await;

    peer_stop.cancel();
    assert_eq!(down(&mut fixture.observed).await.reason, DownReason::Normal);
    peer_task
        .await
        .expect("peer task joined")
        .expect("peer stopped cleanly");
    observer_task.abort();
}

#[tokio::test]
async fn two_observers_receive_the_same_down() {
    let mut fixture = fixture(false);
    let factory = RunnableActorFactory::new();
    let (second_observed_tx, mut second_observed) = mpsc::unbounded_channel();
    let (second_started_tx, mut second_started) = mpsc::unbounded_channel();
    let (second_observer, _) = factory.actor(
        "second-observer",
        Observer {
            peer: fixture.peer_ref.clone(),
            observed: second_observed_tx,
            started: second_started_tx,
            cancel_monitor: false,
        },
    );
    let first_observer = fixture.observer.clone();
    let first_task = tokio::spawn(async move {
        first_observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    let second_task = tokio::spawn(async move {
        second_observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    let peer = fixture.peer.clone();
    let peer_task =
        tokio::spawn(async move { peer.run_until(pending::<()>(), RebindPolicy::Never).await });
    started(&mut fixture.peer_started).await;
    started(&mut fixture.observer_started).await;
    started(&mut second_started).await;

    fixture
        .peer_ref
        .send(PeerMessage::Stop)
        .await
        .expect("stop command sent");
    let first = down(&mut fixture.observed).await;
    let second = down(&mut second_observed).await;
    assert_eq!(first, second);

    peer_task
        .await
        .expect("peer task joined")
        .expect("peer stopped cleanly");
    first_task.abort();
    second_task.abort();
}

#[derive(Clone)]
struct GatedObserver {
    peer: ActorRef<PeerMessage>,
    gate: Arc<Notify>,
    monitor: mpsc::UnboundedSender<MonitorRef>,
    observed: mpsc::UnboundedSender<Down>,
}

impl RawActor for GatedObserver {
    type Msg = Down;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        let monitor = ctx.monitor(&self.peer, |down| down);
        self.monitor.send(monitor).expect("monitor receiver alive");
        self.gate.notified().await;
        if let Some(down) = ctx.recv().await {
            self.observed.send(down).expect("observer receiver alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn cloned_monitor_cancels_and_cannot_retract_accepted_down() {
    let factory = RunnableActorFactory::new();
    let (peer_started_tx, mut peer_started) = mpsc::unbounded_channel();
    let (peer, peer_ref) = factory.actor(
        "peer",
        Peer {
            started: peer_started_tx,
        },
    );
    let gate = Arc::new(Notify::new());
    let (monitor_tx, mut monitor_rx) = mpsc::unbounded_channel();
    let (observed_tx, mut observed) = mpsc::unbounded_channel();
    let (observer, observer_ref) = factory.actor(
        "observer",
        GatedObserver {
            peer: peer_ref.clone(),
            gate: Arc::clone(&gate),
            monitor: monitor_tx,
            observed: observed_tx,
        },
    );
    let peer_task =
        tokio::spawn(async move { peer.run_until(pending::<()>(), RebindPolicy::Never).await });
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut peer_started).await;
    let monitor = monitor_rx.recv().await.expect("monitor created");
    let clone = monitor.clone();
    assert!(!monitor.is_cancelled());

    peer_ref
        .send(PeerMessage::Stop)
        .await
        .expect("stop command sent");
    timeout(Duration::from_secs(1), async {
        while observer_ref.stats().messages_accepted == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("down accepted by observer mailbox");
    clone.cancel();
    assert!(monitor.is_cancelled());
    gate.notify_one();
    assert_eq!(down(&mut observed).await.reason, DownReason::Normal);

    peer_task
        .await
        .expect("peer task joined")
        .expect("peer stopped cleanly");
    observer_task
        .await
        .expect("observer task joined")
        .expect("observer stopped cleanly");
}

#[derive(Clone)]
struct StubbornPeer {
    started: mpsc::UnboundedSender<()>,
}

impl RawActor for StubbornPeer {
    type Msg = ();

    async fn run(&mut self, _ctx: ActorContext<Self::Msg>) -> ActorResult {
        self.started.send(()).expect("start receiver alive");
        pending().await
    }
}

#[derive(Clone)]
struct UnitObserver {
    peer: ActorRef<()>,
    observed: mpsc::UnboundedSender<Down>,
    started: mpsc::UnboundedSender<()>,
}

impl RawActor for UnitObserver {
    type Msg = Down;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        ctx.monitor(&self.peer, |down| down);
        self.started.send(()).expect("start receiver alive");
        while let Some(down) = ctx.recv().await {
            self.observed.send(down).expect("observer receiver alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn supervisor_abort_delivers_failure_down() {
    let mut builder = GraphBuilder::new();
    let (peer_slot, peer_ref) = builder.slot("peer");
    let (peer_started_tx, mut peer_started) = mpsc::unbounded_channel();
    builder.define(
        peer_slot,
        StubbornPeer {
            started: peer_started_tx,
        },
    );
    let (observed_tx, mut observed) = mpsc::unbounded_channel();
    let (observer_started_tx, mut observer_started) = mpsc::unbounded_channel();
    builder.actor(
        "observer",
        UnitObserver {
            peer: peer_ref.clone(),
            observed: observed_tx,
            started: observer_started_tx,
        },
    );
    let graph = builder.build().expect("valid graph");
    let runtime = SupervisedActors::new(graph)
        .actor_shutdown(&peer_ref, ShutdownPolicy::abort())
        .build_runtime(SupervisorBuilder::new())
        .expect("runtime builds");
    let handle = runtime.spawn();
    started(&mut peer_started).await;
    started(&mut observer_started).await;

    handle
        .remove_child("peer")
        .await
        .expect("peer removed by abort");
    let notification = down(&mut observed).await;
    assert_eq!(notification.actor_id, "peer");
    assert_eq!(notification.reason, DownReason::Failure);

    handle
        .shutdown_and_wait()
        .await
        .expect("runtime stopped cleanly");
}

#[derive(Clone)]
struct PanickingMapper {
    peer: ActorRef<PeerMessage>,
    started: mpsc::UnboundedSender<()>,
    mapped: mpsc::UnboundedSender<()>,
}

impl RawActor for PanickingMapper {
    type Msg = ();

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        let mapped = self.mapped.clone();
        ctx.monitor(&self.peer, move |_down| {
            mapped.send(()).expect("mapping receiver alive");
            panic!("deliberate mapping panic")
        });
        self.started.send(()).expect("start receiver alive");
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[tokio::test]
async fn mapping_panic_does_not_change_target_exit() {
    let factory = RunnableActorFactory::new();
    let (peer_started_tx, mut peer_started) = mpsc::unbounded_channel();
    let (peer, peer_ref) = factory.actor(
        "peer",
        Peer {
            started: peer_started_tx,
        },
    );
    let (observer_started_tx, mut observer_started) = mpsc::unbounded_channel();
    let (mapped_tx, mut mapped_rx) = mpsc::unbounded_channel();
    let (observer, _) = factory.actor(
        "observer",
        PanickingMapper {
            peer: peer_ref.clone(),
            started: observer_started_tx,
            mapped: mapped_tx,
        },
    );
    let peer_task =
        tokio::spawn(async move { peer.run_until(pending::<()>(), RebindPolicy::Never).await });
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut peer_started).await;
    started(&mut observer_started).await;

    peer_ref
        .send(PeerMessage::Stop)
        .await
        .expect("stop command sent");
    peer_task
        .await
        .expect("peer task joined")
        .expect("mapping panic did not affect clean peer exit");
    timeout(Duration::from_secs(1), mapped_rx.recv())
        .await
        .expect("mapping closure ran")
        .expect("mapping sender alive");
    observer_task.abort();
}

#[tokio::test]
async fn pending_target_can_be_dropped_from_non_runtime_thread() {
    let factory = RunnableActorFactory::new();
    let (peer, peer_ref) = factory.actor(
        "peer",
        Peer {
            started: mpsc::unbounded_channel().0,
        },
    );
    let (observed_tx, mut observed) = mpsc::unbounded_channel();
    let (observer_started_tx, mut observer_started) = mpsc::unbounded_channel();
    let (observer, _) = factory.actor(
        "observer",
        Observer {
            peer: peer_ref,
            observed: observed_tx,
            started: observer_started_tx,
            cancel_monitor: false,
        },
    );
    let observer_task = tokio::spawn(async move {
        observer
            .run_until(pending::<()>(), RebindPolicy::Never)
            .await
    });
    started(&mut observer_started).await;

    std::thread::spawn(move || drop(peer))
        .join()
        .expect("dropping target outside Tokio does not panic");
    assert_eq!(down(&mut observed).await.reason, DownReason::NoProcess);
    observer_task.abort();
}
