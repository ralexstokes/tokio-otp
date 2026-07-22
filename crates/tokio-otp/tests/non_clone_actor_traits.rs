use std::{
    future::pending,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::mpsc;
use tokio_otp::{
    Actor, ActorContext, ActorResult, GraphBuilder, RawActor, RebindPolicy, Reply, RestartPolicy,
    Runtime,
};

struct HandlerWithNonCloneState {
    _state: Mutex<()>,
}

impl Actor for HandlerWithNonCloneState {
    type Msg = ();

    async fn handle(&mut self, (): (), _ctx: &ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

struct RawWithNonCloneState {
    _state: Mutex<()>,
}

impl RawActor for RawWithNonCloneState {
    type Msg = ();

    async fn run(&mut self, _ctx: ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

fn assert_actor<T: Actor>() {}
fn assert_raw_actor<T: RawActor>() {}

#[test]
fn actor_traits_accept_non_clone_state() {
    assert_actor::<HandlerWithNonCloneState>();
    assert_raw_actor::<HandlerWithNonCloneState>();
    assert_raw_actor::<RawWithNonCloneState>();
}

enum ProbeMsg {
    Increment(Reply<(usize, usize)>),
    Crash,
}

struct NonCloneHandler {
    _guard: Mutex<()>,
    incarnation: usize,
    local: usize,
}

impl Actor for NonCloneHandler {
    type Msg = ProbeMsg;

    async fn handle(&mut self, message: ProbeMsg, _ctx: &ActorContext<ProbeMsg>) -> ActorResult {
        match message {
            ProbeMsg::Increment(reply) => {
                self.local += 1;
                reply.send((self.incarnation, self.local));
                Ok(())
            }
            ProbeMsg::Crash => Err(io::Error::other("restart probe").into()),
        }
    }
}

#[tokio::test]
async fn non_clone_actor_factory_constructs_fresh_state_per_incarnation() {
    let constructions = Arc::new(AtomicUsize::new(0));
    let mut builder = GraphBuilder::new();
    let actor_ref = builder.actor("handler", {
        let constructions = constructions.clone();
        move || NonCloneHandler {
            _guard: Mutex::new(()),
            incarnation: constructions.fetch_add(1, Ordering::SeqCst),
            local: 0,
        }
    });
    let handle = Runtime::builder()
        .graph(builder.build().expect("graph builds"))
        .restart(RestartPolicy::OnFailure)
        .build()
        .expect("runtime builds")
        .spawn();

    assert_eq!(
        actor_ref
            .call(ProbeMsg::Increment)
            .await
            .expect("first incarnation replies"),
        (0, 1)
    );
    let restarted = handle
        .monitor_restart("handler")
        .expect("restart monitor exists");
    actor_ref
        .send(ProbeMsg::Crash)
        .await
        .expect("crash accepted");
    tokio::time::timeout(Duration::from_secs(1), restarted)
        .await
        .expect("restart observed")
        .expect("restart succeeds");
    assert_eq!(
        actor_ref
            .call(ProbeMsg::Increment)
            .await
            .expect("replacement replies"),
        (1, 1)
    );
    assert_eq!(constructions.load(Ordering::SeqCst), 2);

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

struct NonCloneRaw {
    _guard: Mutex<()>,
    incarnation: usize,
    observed: mpsc::UnboundedSender<(usize, usize)>,
}

impl RawActor for NonCloneRaw {
    type Msg = bool;

    async fn run(&mut self, mut ctx: ActorContext<bool>) -> ActorResult {
        let mut local = 0;
        while let Some(crash) = ctx.recv().await {
            if crash {
                return Err(io::Error::other("restart probe").into());
            }
            local += 1;
            self.observed
                .send((self.incarnation, local))
                .expect("observer alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn non_clone_raw_actor_factory_is_reused_for_restart() {
    let constructions = Arc::new(AtomicUsize::new(0));
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let mut builder = GraphBuilder::new();
    let actor_ref = builder.actor("raw", {
        let constructions = constructions.clone();
        move || NonCloneRaw {
            _guard: Mutex::new(()),
            incarnation: constructions.fetch_add(1, Ordering::SeqCst),
            observed: observed_tx.clone(),
        }
    });
    let handle = Runtime::builder()
        .graph(builder.build().expect("graph builds"))
        .restart(RestartPolicy::OnFailure)
        .build()
        .expect("runtime builds")
        .spawn();

    actor_ref.send(false).await.expect("first message accepted");
    assert_eq!(observed_rx.recv().await, Some((0, 1)));
    let restarted = handle
        .monitor_restart("raw")
        .expect("restart monitor exists");
    actor_ref.send(true).await.expect("crash accepted");
    tokio::time::timeout(Duration::from_secs(1), restarted)
        .await
        .expect("restart observed")
        .expect("restart succeeds");
    actor_ref
        .send(false)
        .await
        .expect("replacement message accepted");
    assert_eq!(observed_rx.recv().await, Some((1, 1)));
    assert_eq!(constructions.load(Ordering::SeqCst), 2);

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[tokio::test]
async fn constructor_panic_uses_the_actor_panic_path() {
    let mut builder = GraphBuilder::new();
    builder.actor("panics", || -> RawWithNonCloneState {
        panic!("constructor panic")
    });
    let graph = builder.build().expect("registration does not construct");
    let actor = graph.actors()[0].clone();

    let joined =
        tokio::spawn(async move { actor.run_until(pending::<()>(), RebindPolicy::Never).await })
            .await;
    assert!(joined.expect_err("constructor panic propagates").is_panic());
}
