use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{sync::mpsc, time::timeout};
use tokio_otp::{ActorContext, ActorRef, ActorResult, BoxError, GraphBuilder, RawActor, Runtime};
use tokio_supervisor::{Strategy, SupervisorBuilder};

fn build_runtime<A>(actor: A) -> (Runtime, ActorRef<A::Msg>)
where
    A: RawActor,
{
    let mut builder = GraphBuilder::new();
    let actor_ref = builder.actor("timer", actor);
    let graph = builder.build().expect("valid graph");
    let runtime = tokio_otp::SupervisedActors::new(graph)
        .build_runtime(SupervisorBuilder::new().strategy(Strategy::OneForOne))
        .expect("runtime builds");
    (runtime, actor_ref)
}

#[derive(Clone)]
struct OneShot {
    observed: mpsc::UnboundedSender<&'static str>,
}

impl RawActor for OneShot {
    type Msg = &'static str;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        let _timer = ctx.send_after("tick", Duration::from_millis(20));
        while let Some(message) = ctx.recv().await {
            self.observed.send(message).expect("observer alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn send_after_fires_once() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, _) = build_runtime(OneShot {
        observed: observed_tx,
    });
    let handle = runtime.spawn();

    assert_eq!(
        timeout(Duration::from_secs(1), observed_rx.recv())
            .await
            .expect("timer fired"),
        Some("tick")
    );
    assert!(
        timeout(Duration::from_millis(60), observed_rx.recv())
            .await
            .is_err(),
        "one-shot timer must not fire twice"
    );

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[derive(Clone)]
struct CancelledTimer {
    observed: mpsc::UnboundedSender<&'static str>,
}

impl RawActor for CancelledTimer {
    type Msg = &'static str;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        let timer = ctx.send_after("cancelled", Duration::from_millis(20));
        timer.cancel();
        assert!(timer.is_cancelled());
        while let Some(message) = ctx.recv().await {
            self.observed.send(message).expect("observer alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn cancelling_send_after_prevents_delivery() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, _) = build_runtime(CancelledTimer {
        observed: observed_tx,
    });
    let handle = runtime.spawn();

    assert!(
        timeout(Duration::from_millis(60), observed_rx.recv())
            .await
            .is_err(),
        "cancelled timer delivered a message"
    );

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[derive(Clone)]
struct Interval {
    observed: mpsc::UnboundedSender<usize>,
}

impl RawActor for Interval {
    type Msg = ();

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        let timer = ctx.interval((), Duration::from_millis(10));
        let mut ticks = 0;
        while ctx.recv().await.is_some() {
            ticks += 1;
            self.observed.send(ticks).expect("observer alive");
            if ticks == 3 {
                timer.cancel();
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn interval_repeats_until_cancelled() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, _) = build_runtime(Interval {
        observed: observed_tx,
    });
    let handle = runtime.spawn();

    for expected in 1..=3 {
        assert_eq!(
            timeout(Duration::from_secs(1), observed_rx.recv())
                .await
                .expect("interval ticked"),
            Some(expected)
        );
    }
    assert!(
        timeout(Duration::from_millis(50), observed_rx.recv())
            .await
            .is_err(),
        "interval continued after cancellation"
    );

    handle.shutdown_and_wait().await.expect("clean shutdown");
}

#[derive(Clone)]
struct RestartingTimer {
    runs: Arc<AtomicUsize>,
    observed: mpsc::UnboundedSender<&'static str>,
}

impl RawActor for RestartingTimer {
    type Msg = &'static str;

    async fn run(&mut self, mut ctx: ActorContext<Self::Msg>) -> ActorResult {
        if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
            let _old_timer = ctx.send_after("old", Duration::from_millis(150));
            return Err::<(), BoxError>(Box::new(io::Error::other("restart")));
        }

        let _new_timer = ctx.send_after("new", Duration::from_millis(10));
        while let Some(message) = ctx.recv().await {
            self.observed.send(message).expect("observer alive");
        }
        Ok(())
    }
}

#[tokio::test]
async fn restart_cancels_previous_incarnations_timers() {
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let (runtime, _) = build_runtime(RestartingTimer {
        runs: Arc::new(AtomicUsize::new(0)),
        observed: observed_tx,
    });
    let handle = runtime.spawn();

    assert_eq!(
        timeout(Duration::from_secs(1), observed_rx.recv())
            .await
            .expect("new incarnation timer fired"),
        Some("new")
    );
    assert!(
        timeout(Duration::from_millis(200), observed_rx.recv())
            .await
            .is_err(),
        "a previous incarnation delivered a stale timer message"
    );

    handle.shutdown_and_wait().await.expect("clean shutdown");
}
