use std::sync::Arc;

use tokio::sync::{Notify, mpsc};
use tokio_otp::{ActorContext, ActorResult, GraphBuilder, MailboxMode, RawActor, RebindPolicy};
use tokio_util::sync::CancellationToken;

struct GatedCollector<M> {
    started: mpsc::UnboundedSender<()>,
    release: Arc<Notify>,
    received: mpsc::UnboundedSender<M>,
}

impl<M> Clone for GatedCollector<M> {
    fn clone(&self) -> Self {
        Self {
            started: self.started.clone(),
            release: Arc::clone(&self.release),
            received: self.received.clone(),
        }
    }
}

impl<M: Send + 'static> RawActor for GatedCollector<M> {
    type Msg = M;

    async fn run(&mut self, mut ctx: ActorContext<M>) -> ActorResult {
        self.started.send(()).expect("test receives start signal");
        self.release.notified().await;
        while let Some(message) = ctx.recv().await {
            self.received
                .send(message)
                .expect("test receives actor messages");
        }
        Ok(())
    }
}

#[tokio::test]
async fn conflate_keeps_only_the_newest_unread_message() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (received_tx, mut received_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let mut builder = GraphBuilder::new();
    let actor_ref = builder.actor_with_mailbox(
        "ticks",
        GatedCollector {
            started: started_tx,
            release: Arc::clone(&release),
            received: received_tx,
        },
        MailboxMode::Conflate,
    );
    let graph = builder.build().expect("valid graph");
    let actor = graph.actors()[0].clone();
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let stop = stop.clone();
        async move { actor.run_until(stop.cancelled(), RebindPolicy::Never).await }
    });
    started_rx.recv().await.expect("actor started");

    for tick in 0..100 {
        if tick % 2 == 0 {
            actor_ref
                .send(tick)
                .await
                .expect("conflating send succeeds");
        } else {
            actor_ref
                .try_send(tick)
                .expect("conflating try_send succeeds");
        }
    }

    let stats = actor_ref.stats();
    assert_eq!(stats.messages_accepted, 100);
    assert_eq!(stats.messages_conflated, 99);
    assert_eq!(stats.mailbox_depth, 1);
    assert_eq!(stats.mailbox_capacity, 1);

    release.notify_one();
    assert_eq!(received_rx.recv().await, Some(99));
    assert!(received_rx.try_recv().is_err());

    stop.cancel();
    task.await.expect("actor task joins").expect("actor stops");
}

#[derive(Debug, Eq, PartialEq)]
struct Tick {
    symbol: &'static str,
    price: u64,
}

#[tokio::test]
async fn conflate_by_key_preserves_latest_values_in_first_key_order() {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (received_tx, mut received_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let mut builder = GraphBuilder::new();
    builder.mailbox_capacity(4);
    let (slot, actor_ref) = builder.slot_with_mailbox(
        "market-data",
        MailboxMode::conflate_by_key(|tick: &Tick| tick.symbol),
    );
    builder.define(
        slot,
        GatedCollector {
            started: started_tx,
            release: Arc::clone(&release),
            received: received_tx,
        },
    );
    let graph = builder.build().expect("valid graph");
    let actor = graph.actors()[0].clone();
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let stop = stop.clone();
        async move { actor.run_until(stop.cancelled(), RebindPolicy::Never).await }
    });
    started_rx.recv().await.expect("actor started");

    for tick in [
        Tick {
            symbol: "ETH",
            price: 10,
        },
        Tick {
            symbol: "BTC",
            price: 20,
        },
        Tick {
            symbol: "ETH",
            price: 11,
        },
        Tick {
            symbol: "BTC",
            price: 21,
        },
    ] {
        actor_ref.send(tick).await.expect("keyed send succeeds");
    }

    let stats = actor_ref.stats();
    assert_eq!(stats.messages_accepted, 4);
    assert_eq!(stats.messages_conflated, 2);
    assert_eq!(stats.mailbox_depth, 2);
    assert_eq!(stats.mailbox_capacity, 4);

    release.notify_one();
    assert_eq!(
        received_rx.recv().await,
        Some(Tick {
            symbol: "ETH",
            price: 11,
        })
    );
    assert_eq!(
        received_rx.recv().await,
        Some(Tick {
            symbol: "BTC",
            price: 21,
        })
    );
    assert!(received_rx.try_recv().is_err());

    stop.cancel();
    task.await.expect("actor task joins").expect("actor stops");
}
