use std::{
    future::pending,
    io,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, mpsc},
    time::timeout,
};
use tokio_otp::{
    ActorContext, ActorOptions, ActorRef, ActorResult, ChildSpec, ControlError,
    DynamicActorOptions, GraphBuilder, MailboxMode, MessageSize, MonitorEvent, MonitorRef,
    RawActor, RestartPolicy, Runtime, RuntimeHandle, SendError, ShutdownPolicy, Strategy,
    SupervisedActors, SupervisorBuilder,
};

fn build_runtime(graph: tokio_otp::Graph) -> Runtime {
    SupervisedActors::new(graph)
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("runtime builds")
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

impl<M: Send + 'static> RawActor for Drain<M> {
    type Msg = M;

    async fn run(&mut self, mut ctx: ActorContext<M>) -> ActorResult {
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[derive(Clone)]
struct GatedExit {
    release: Arc<Notify>,
    fail: bool,
}

impl RawActor for GatedExit {
    type Msg = ();

    async fn run(&mut self, _ctx: ActorContext<()>) -> ActorResult {
        self.release.notified().await;
        if self.fail {
            Err(io::Error::other("dynamic actor failed").into())
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
struct RestartOnce {
    starts: Arc<AtomicUsize>,
}

impl RawActor for RestartOnce {
    type Msg = ();

    async fn run(&mut self, ctx: ActorContext<()>) -> ActorResult {
        if self.starts.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(io::Error::other("restart me").into())
        } else {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }
    }
}

enum WatchMsg {
    Watch(ActorRef<()>),
    Event(MonitorEvent),
}

#[derive(Clone)]
struct Watcher {
    observed: mpsc::UnboundedSender<MonitorEvent>,
}

impl RawActor for Watcher {
    type Msg = WatchMsg;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        let mut watch: Option<MonitorRef> = None;
        while let Some(message) = ctx.recv().await {
            match message {
                WatchMsg::Watch(target) => {
                    watch = Some(ctx.watch(&target, WatchMsg::Event));
                }
                WatchMsg::Event(event) => {
                    self.observed
                        .send(event)
                        .expect("monitor receiver remains alive");
                }
            }
        }
        drop(watch);
        Ok(())
    }
}

async fn wait_for_child(handle: &RuntimeHandle, id: &str, present: bool) {
    timeout(Duration::from_secs(1), async {
        let mut snapshots = handle.subscribe_snapshots();
        loop {
            if snapshots.borrow().child(id).is_some() == present {
                return;
            }
            snapshots
                .changed()
                .await
                .expect("runtime remains available");
        }
    })
    .await
    .expect("child membership reached expected state");
}

async fn next_monitor_event(events: &mut mpsc::UnboundedReceiver<MonitorEvent>) -> MonitorEvent {
    timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("monitor event arrived")
        .expect("monitor sender remains alive")
}

struct SizedMessage(Vec<u8>);

impl MessageSize for SizedMessage {
    fn size_hint(&self) -> usize {
        self.0.len()
    }
}

#[derive(Clone)]
struct Observe {
    observed: mpsc::UnboundedSender<String>,
}

impl RawActor for Observe {
    type Msg = String;

    async fn run(&mut self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            self.observed.send(message).expect("receiver alive");
        }
        Ok(())
    }
}

enum ForwardMsg {
    Target(ActorRef<String>),
    Forward(String),
}

#[derive(Clone)]
struct Forwarder;

impl RawActor for Forwarder {
    type Msg = ForwardMsg;

    async fn run(&mut self, mut ctx: ActorContext<ForwardMsg>) -> ActorResult {
        let mut target = None;
        while let Some(message) = ctx.recv().await {
            match message {
                ForwardMsg::Target(actor_ref) => target = Some(actor_ref),
                ForwardMsg::Forward(message) => {
                    target
                        .as_ref()
                        .expect("target distributed before forwarding")
                        .send(message)
                        .await?
                }
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn graphless_runtime_adds_removes_and_readds_actors() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let handle = Runtime::builder()
        .build()
        .expect("graphless runtime builds")
        .spawn();

    assert!(handle.snapshot().children.is_empty());
    let sink = handle
        .add_actor(
            "sink",
            {
                let observed_tx = observed_tx.clone();
                move || Observe {
                    observed: observed_tx.clone(),
                }
            },
            DynamicActorOptions::default(),
        )
        .await
        .expect("sink added");
    sink.send("first".to_owned()).await.expect("message sent");
    assert_eq!(observed_rx.recv().await.as_deref(), Some("first"));
    let initial_epoch = handle
        .snapshot()
        .child("sink")
        .expect("sink snapshot available")
        .membership_epoch;
    assert_eq!(
        handle
            .actor_stats()
            .into_iter()
            .find(|stats| stats.actor_id == "sink")
            .expect("sink stats available")
            .membership_epoch,
        Some(initial_epoch)
    );
    assert_eq!(
        sink.stats().membership_epoch,
        None,
        "standalone ref stats have no supervisor context"
    );

    handle.remove_child("sink").await.expect("sink removed");
    assert!(matches!(
        sink.send("after-remove".to_owned()).await,
        Err(SendError::ActorTerminated { actor_id , .. }) if actor_id == "sink"
    ));

    let replacement = handle
        .add_actor(
            "sink",
            move || Observe {
                observed: observed_tx.clone(),
            },
            DynamicActorOptions::default(),
        )
        .await
        .expect("label can be reused");
    replacement
        .send("second".to_owned())
        .await
        .expect("replacement receives");
    assert_eq!(observed_rx.recv().await.as_deref(), Some("second"));
    let replacement_snapshot_epoch = handle
        .snapshot()
        .child("sink")
        .expect("replacement snapshot available")
        .membership_epoch;
    let replacement_epoch = handle
        .actor_stats()
        .into_iter()
        .find(|stats| stats.actor_id == "sink")
        .expect("replacement stats available")
        .membership_epoch
        .expect("runtime stats include supervisor membership");
    assert_eq!(replacement_epoch, replacement_snapshot_epoch);
    assert!(replacement_epoch > initial_epoch);

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn never_actor_auto_removal_preserves_monitor_order_and_reuses_id() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut graph = GraphBuilder::new();
    let watcher = graph.actor("watcher", move || Watcher {
        observed: observed_tx.clone(),
    });
    let handle = build_runtime(graph.build().expect("valid graph")).spawn();
    let release = Arc::new(Notify::new());
    let target = handle
        .add_actor(
            "temporary",
            {
                let release = release.clone();
                move || GatedExit {
                    release: release.clone(),
                    fail: false,
                }
            },
            DynamicActorOptions::new().restart(RestartPolicy::Never),
        )
        .await
        .expect("temporary actor added");

    watcher
        .send(WatchMsg::Watch(target.clone()))
        .await
        .expect("watch requested");
    assert!(matches!(
        next_monitor_event(&mut observed_rx).await,
        MonitorEvent::Up { ref actor_id, .. } if actor_id == "temporary"
    ));

    release.notify_one();
    assert!(matches!(
        next_monitor_event(&mut observed_rx).await,
        MonitorEvent::Down(ref down) if down.actor_id == "temporary"
    ));
    assert!(matches!(
        next_monitor_event(&mut observed_rx).await,
        MonitorEvent::Terminated { ref actor_id, .. } if actor_id == "temporary"
    ));
    wait_for_child(&handle, "temporary", false).await;
    assert!(matches!(
        target.send(()).await,
        Err(SendError::ActorTerminated { actor_id, .. }) if actor_id == "temporary"
    ));

    handle
        .add_actor("temporary", Drain::<()>::new, DynamicActorOptions::new())
        .await
        .expect("auto-removed actor id is reusable");
    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn never_actor_auto_removes_after_failure() {
    let handle = Runtime::builder()
        .build()
        .expect("graphless runtime builds")
        .spawn();
    let release = Arc::new(Notify::new());
    let target = handle
        .add_actor(
            "temporary",
            {
                let release = release.clone();
                move || GatedExit {
                    release: release.clone(),
                    fail: true,
                }
            },
            DynamicActorOptions::new().restart(RestartPolicy::Never),
        )
        .await
        .expect("temporary actor added");

    release.notify_one();
    wait_for_child(&handle, "temporary", false).await;
    assert!(matches!(
        target.send(()).await,
        Err(SendError::ActorTerminated { actor_id, .. }) if actor_id == "temporary"
    ));
    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn remove_on_exit_defaults_and_overrides_follow_the_final_restart_policy() {
    let handle = Runtime::builder()
        .build()
        .expect("graphless runtime builds")
        .spawn();

    let retained_release = Arc::new(Notify::new());
    handle
        .add_actor(
            "transient-default",
            {
                let release = retained_release.clone();
                move || GatedExit {
                    release: release.clone(),
                    fail: false,
                }
            },
            DynamicActorOptions::new().restart(RestartPolicy::OnFailure),
        )
        .await
        .expect("transient actor added");
    retained_release.notify_one();
    wait_for_child(&handle, "transient-default", true).await;
    timeout(Duration::from_secs(1), async {
        let mut snapshots = handle.subscribe_snapshots();
        loop {
            if snapshots
                .borrow()
                .child("transient-default")
                .is_some_and(|child| child.last_exit.is_some())
            {
                break;
            }
            snapshots
                .changed()
                .await
                .expect("runtime remains available");
        }
    })
    .await
    .expect("transient exit recorded");

    let removed_release = Arc::new(Notify::new());
    handle
        .add_actor(
            "transient-override",
            {
                let release = removed_release.clone();
                move || GatedExit {
                    release: release.clone(),
                    fail: false,
                }
            },
            DynamicActorOptions::new()
                .remove_on_exit(true)
                .restart(RestartPolicy::OnFailure),
        )
        .await
        .expect("transient override actor added");
    removed_release.notify_one();
    wait_for_child(&handle, "transient-override", false).await;

    let never_release = Arc::new(Notify::new());
    handle
        .add_actor(
            "never-override",
            {
                let release = never_release.clone();
                move || GatedExit {
                    release: release.clone(),
                    fail: false,
                }
            },
            DynamicActorOptions::new()
                .remove_on_exit(false)
                .restart(RestartPolicy::Never),
        )
        .await
        .expect("never override actor added");
    never_release.notify_one();
    timeout(Duration::from_secs(1), async {
        let mut snapshots = handle.subscribe_snapshots();
        loop {
            if snapshots
                .borrow()
                .child("never-override")
                .is_some_and(|child| child.last_exit.is_some())
            {
                break;
            }
            snapshots
                .changed()
                .await
                .expect("runtime remains available");
        }
    })
    .await
    .expect("never exit retained by override");

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn remove_on_exit_does_not_remove_an_actor_that_restarts() {
    let handle = Runtime::builder()
        .build()
        .expect("graphless runtime builds")
        .spawn();
    let starts = Arc::new(AtomicUsize::new(0));
    handle
        .add_actor(
            "restart-once",
            {
                let starts = starts.clone();
                move || RestartOnce {
                    starts: starts.clone(),
                }
            },
            DynamicActorOptions::new()
                .restart(RestartPolicy::OnFailure)
                .remove_on_exit(true),
        )
        .await
        .expect("restartable actor added");

    timeout(Duration::from_secs(1), async {
        let mut snapshots = handle.subscribe_snapshots();
        loop {
            if snapshots
                .borrow()
                .child("restart-once")
                .is_some_and(|child| child.generation >= 1)
            {
                break;
            }
            snapshots
                .changed()
                .await
                .expect("runtime remains available");
        }
    })
    .await
    .expect("actor restarted");
    assert_eq!(starts.load(Ordering::SeqCst), 2);

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn runtime_added_actor_can_observe_message_sizes() {
    let handle = Runtime::builder()
        .build()
        .expect("graphless runtime builds")
        .spawn();
    let sink = handle
        .add_actor_with_options(
            "sink",
            Drain::<SizedMessage>::new,
            ActorOptions::new().message_size(),
            DynamicActorOptions::default(),
        )
        .await
        .expect("sized actor added");

    sink.send(SizedMessage(vec![0; 12]))
        .await
        .expect("message sent");
    let stats = handle
        .actor_stats()
        .into_iter()
        .find(|stats| stats.actor_id == "sink")
        .expect("dynamic actor stats available");
    assert_eq!(stats.message_bytes_accepted, Some(12));

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[derive(Clone)]
struct GatedDrain {
    release: Arc<Notify>,
}

impl RawActor for GatedDrain {
    type Msg = u64;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        self.release.notified().await;
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[tokio::test]
async fn runtime_added_actor_uses_non_default_mailbox_options() {
    let handle = Runtime::builder()
        .build()
        .expect("graphless runtime builds")
        .spawn();
    let release = Arc::new(Notify::new());
    let sink = handle
        .add_actor_with_options(
            "sink",
            {
                let release = release.clone();
                move || GatedDrain {
                    release: release.clone(),
                }
            },
            ActorOptions::new().mailbox(MailboxMode::Conflate),
            DynamicActorOptions::default(),
        )
        .await
        .expect("conflating actor added");

    for message in 0..3 {
        sink.send(message).await.expect("message accepted");
    }
    let stats = sink.stats();
    assert_eq!(stats.messages_accepted, 3);
    assert_eq!(stats.messages_conflated, 2);
    assert_eq!(stats.mailbox_capacity, 1);

    release.notify_one();
    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn runtime_added_ref_is_distributed_to_static_actor_by_message() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let forwarder = builder.actor("forwarder", || Forwarder);
    let handle = build_runtime(builder.build().expect("valid graph")).spawn();

    let sink = handle
        .add_actor(
            "sink",
            move || Observe {
                observed: observed_tx.clone(),
            },
            DynamicActorOptions::default(),
        )
        .await
        .expect("sink added");
    forwarder
        .send(ForwardMsg::Target(sink))
        .await
        .expect("typed ref distributed");
    forwarder
        .send(ForwardMsg::Forward("forwarded".to_owned()))
        .await
        .expect("message forwarded");

    assert_eq!(observed_rx.recv().await.as_deref(), Some("forwarded"));
    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[derive(Clone)]
struct ForwardTo {
    target: ActorRef<String>,
}

impl RawActor for ForwardTo {
    type Msg = String;

    async fn run(&mut self, mut ctx: ActorContext<String>) -> ActorResult {
        while let Some(message) = ctx.recv().await {
            self.target.send(message).await?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn runtime_added_actor_can_receive_static_ref_at_creation() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let sink = builder.actor("sink", move || Observe {
        observed: observed_tx.clone(),
    });
    let handle = build_runtime(builder.build().expect("valid graph")).spawn();

    let dynamic = handle
        .add_actor(
            "dynamic",
            move || ForwardTo {
                target: sink.clone(),
            },
            DynamicActorOptions::default(),
        )
        .await
        .expect("dynamic actor added");
    dynamic
        .send("forwarded".to_owned())
        .await
        .expect("dynamic actor receives");
    assert_eq!(observed_rx.recv().await.as_deref(), Some("forwarded"));

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[derive(Clone)]
struct PendingActor;

impl RawActor for PendingActor {
    type Msg = ();

    async fn run(&mut self, _ctx: ActorContext<()>) -> ActorResult {
        pending::<()>().await;
        Ok(())
    }
}

#[tokio::test]
async fn timed_out_removal_terminates_the_typed_ref() {
    let handle = Runtime::builder().build().expect("runtime builds").spawn();
    let actor_ref = handle
        .add_actor(
            "dynamic",
            || PendingActor,
            DynamicActorOptions::new().shutdown(ShutdownPolicy::cooperative_strict(
                Duration::from_millis(20),
            )),
        )
        .await
        .expect("actor added");

    assert!(matches!(
        handle.remove_child("dynamic").await,
        Err(ControlError::ShutdownTimedOut(actor_id)) if actor_id == "dynamic"
    ));
    assert!(
        handle
            .actor_stats()
            .iter()
            .all(|stats| stats.actor_id != "dynamic"),
        "timed-out removal immediately forgets actor stats"
    );
    assert!(matches!(
        actor_ref.send(()).await,
        Err(SendError::ActorTerminated { actor_id , .. }) if actor_id == "dynamic"
    ));

    handle
        .add_actor("dynamic", Drain::<()>::new, DynamicActorOptions::default())
        .await
        .expect("label reusable after timed-out removal");
    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn runtime_new_supports_runtime_added_actors() {
    let supervisor = SupervisorBuilder::new()
        .child(ChildSpec::new("seed", |ctx| async move {
            ctx.shutdown_token().cancelled().await;
            Ok(())
        }))
        .build()
        .expect("valid supervisor");
    let handle = Runtime::new(supervisor).spawn();

    let actor_ref = handle
        .add_actor("dynamic", Drain::<()>::new, DynamicActorOptions::default())
        .await
        .expect("all runtimes support actor creation");
    assert_eq!(actor_ref.id(), "dynamic");

    timeout(Duration::from_secs(1), handle.shutdown_and_wait())
        .await
        .expect("shutdown completed")
        .expect("clean shutdown");
}
