mod control;
mod health;
mod ledger;
mod messages;
mod reconciler;
mod router;
mod telemetry;
mod venue;

use std::{
    collections::HashMap,
    error::Error,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use metrics_util::debugging::Snapshotter;
use tokio_otp::{SupervisedActors, SupervisorBuilder, prelude::*};

use control::Control;
use health::Health;
use ledger::Ledger;
use messages::*;
use reconciler::Reconciler;
use router::OrderRouter;
use telemetry::LatencyRecorder;
use venue::{ExchangeSim, VenueFeed, VenueGateway};

const VENUE_A: VenueId = "venue-a";
const VENUE_B: VenueId = "venue-b";
const INIT_TIMEOUT: Duration = Duration::from_secs(2);
const PHASE_TIMEOUT: Duration = Duration::from_secs(3);
const URGENT_BOUND: Duration = Duration::from_millis(500);

type AnyError = Box<dyn Error + Send + Sync>;

struct App {
    handle: RuntimeHandle,
    venue_graph: Graph,
    venue_a_feed: ActorRef<FeedMsg>,
    venue_b_feed: ActorRef<FeedMsg>,
    router: ActorRef<RouterMsg>,
    ledger: ActorRef<LedgerMsg>,
    reconciler: ActorRef<ReconcilerMsg>,
    control: ActorRef<ControlMsg>,
    health: ActorRef<HealthMsg>,
    venue_a: ExchangeSim,
    venue_b: ExchangeSim,
    intake_gate: Arc<AtomicBool>,
    background_stop: CancellationToken,
    sampler: tokio::task::JoinHandle<()>,
    event_pump: tokio::task::JoinHandle<()>,
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init()?;
    let metrics = telemetry::install_metrics()?;
    let latency = LatencyRecorder::default();
    let app = build_app(latency.clone()).await?;

    phase_0(&app).await?;
    let (order_1, _order_2) = phase_1(&app).await?;
    phase_2(&app).await?;
    phase_3(&app).await?;
    let unknown_no_accept = phase_4(&app).await?;
    let unknown_accepted = phase_5(&app).await?;
    phase_6(&app).await?;
    phase_7(&app).await?;
    phase_8(
        app,
        latency,
        metrics,
        [&order_1, &unknown_no_accept, &unknown_accepted],
    )
    .await?;
    Ok(())
}

async fn build_app(latency: LatencyRecorder) -> Result<App, AnyError> {
    let venue_a = ExchangeSim::default();
    let venue_b = ExchangeSim::default();
    let intake_gate = Arc::new(AtomicBool::new(true));

    let mut core = GraphBuilder::new();
    core.name("trading-core");
    core.mailbox_capacity(32);
    let (reconciler_slot, reconciler) = core.slot::<ReconcilerMsg>("reconciler");
    let (ledger_slot, ledger) = core.slot::<LedgerMsg>("ledger");
    let (router_slot, router) = core.slot::<RouterMsg>("order-router");
    let (control_slot, control) = core.slot::<ControlMsg>("control");
    let (health_slot, health) = core.slot::<HealthMsg>("health");

    let mut venues = GraphBuilder::new();
    venues.name("trading-venues");
    venues.mailbox_capacity(16);
    let venue_a_feed = venues.actor_with_options(
        "venue-a-feed",
        VenueFeed {
            venue: VENUE_A,
            exchange: venue_a.clone(),
            reconciler: reconciler.clone(),
            latency: latency.clone(),
        },
        ActorOptions::new()
            .mailbox(MailboxMode::Conflate)
            .message_size(),
    );
    let venue_a_gateway = venues.actor(
        "venue-a-gateway",
        VenueGateway {
            venue: VENUE_A,
            exchange: venue_a.clone(),
            ledger: ledger.clone(),
            latency: latency.clone(),
        },
    );
    let venue_b_feed = venues.actor_with_options(
        "venue-b-feed",
        VenueFeed {
            venue: VENUE_B,
            exchange: venue_b.clone(),
            reconciler: reconciler.clone(),
            latency: latency.clone(),
        },
        ActorOptions::new()
            .mailbox(MailboxMode::conflate_by_key(
                |message: &FeedMsg| match message {
                    FeedMsg::Tick(snapshot) => snapshot.symbol,
                    FeedMsg::Crash => "__control__",
                },
            ))
            .message_size(),
    );
    let venue_b_gateway = venues.actor(
        "venue-b-gateway",
        VenueGateway {
            venue: VENUE_B,
            exchange: venue_b.clone(),
            ledger: ledger.clone(),
            latency: latency.clone(),
        },
    );

    let feed_refs = HashMap::from([
        (VENUE_A, venue_a_feed.clone()),
        (VENUE_B, venue_b_feed.clone()),
    ]);
    let gateways = HashMap::from([
        (VENUE_A, venue_a_gateway.clone()),
        (VENUE_B, venue_b_gateway.clone()),
    ]);
    core.define(
        reconciler_slot,
        Reconciler::new(
            feed_refs,
            vec![(VENUE_A, venue_a.clone()), (VENUE_B, venue_b.clone())],
        ),
    );
    core.define(ledger_slot, Ledger::new(latency));
    core.define(
        router_slot,
        OrderRouter::new(gateways.clone(), ledger.clone(), intake_gate.clone()),
    );
    core.define(
        control_slot,
        Control {
            gateways: vec![venue_a_gateway, venue_b_gateway],
            intake_gate: intake_gate.clone(),
        },
    );
    core.define(health_slot, Health::new(control.clone()));

    let venue_graph = venues.build()?;
    let core_graph = core.build()?;
    let venue_supervisor = SupervisedActors::new(venue_graph.clone()).build_supervisor(
        SupervisorBuilder::new()
            .strategy(Strategy::OneForOne)
            .start_mode(StartMode::Sequential)
            .restart_intensity(RestartIntensity::new(5, Duration::from_secs(10))),
    )?;
    let runtime = SupervisedActors::new(core_graph).build_runtime(
        SupervisorBuilder::new()
            .strategy(Strategy::OneForOne)
            .start_mode(StartMode::Sequential)
            .supervisor("venues", venue_supervisor),
    )?;
    let handle = runtime.spawn();

    let background_stop = CancellationToken::new();
    let sampler = tokio::spawn(telemetry::sample(
        handle.clone(),
        venue_graph.clone(),
        background_stop.clone(),
    ));
    let mut events = handle.subscribe();
    let event_health = health.clone();
    let event_stop = background_stop.clone();
    let event_pump = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = event_stop.cancelled() => break,
                event = events.recv() => match event {
                    Ok(event) => {
                        let path = event.path();
                        if path.first().is_some_and(|segment| segment.id == "venues")
                            && let SupervisorEvent::ChildRestarted { id, .. } = event.leaf()
                        {
                            let _ = event_health
                                .send(HealthMsg::RestartObserved { child_id: id.clone() })
                                .await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        panic!("event pump lagged by {skipped} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    Ok(App {
        handle,
        venue_graph,
        venue_a_feed,
        venue_b_feed,
        router,
        ledger,
        reconciler,
        control,
        health,
        venue_a,
        venue_b,
        intake_gate,
        background_stop,
        sampler,
        event_pump,
    })
}

async fn phase_0(app: &App) -> Result<(), AnyError> {
    tokio::time::timeout(INIT_TIMEOUT, app.handle.wait_started()).await??;
    assert_eq!(app.venue_a.feed_sessions(VENUE_A), 1);
    assert_eq!(app.venue_b.feed_sessions(VENUE_B), 1);
    assert_eq!(app.venue_a.gateway_sessions(VENUE_A), 1);
    assert_eq!(app.venue_b.gateway_sessions(VENUE_B), 1);
    println!("PHASE 0 OK — readiness-gated startup");
    Ok(())
}

async fn phase_1(app: &App) -> Result<(OrderKey, OrderKey), AnyError> {
    tick(&app.venue_a_feed, VENUE_A, "BTC-USD", 1).await?;
    tick(&app.venue_b_feed, VENUE_B, "BTC-USD", 1).await?;
    await_until(|| async {
        status(&app.reconciler)
            .await
            .is_some_and(|status| both_health(&status, VenueHealth::Fresh))
    })
    .await?;

    let order_1 = expect_placed(submit(&app.router, "OK", 2, VENUE_A).await?)?;
    await_until(|| async {
        ledger_report(&app.ledger).await.is_some_and(|report| {
            report
                .effects
                .get(&order_1)
                .is_some_and(|effects| effects.acknowledgements == 1 && effects.fills == 1)
        })
    })
    .await?;

    let order_2 = expect_placed(submit(&app.router, "OK", 1, VENUE_B).await?)?;
    let cancelled = bounded_call(&app.router, |reply| RouterMsg::Cancel {
        key: order_2.clone(),
        reply,
    })
    .await?;
    assert_eq!(cancelled, CancelOutcome::Cancelled);
    await_until(|| async {
        ledger_report(&app.ledger).await.is_some_and(|report| {
            report
                .effects
                .get(&order_2)
                .is_some_and(|effects| effects.cancellations == 1)
        })
    })
    .await?;
    println!("PHASE 1 OK — deterministic market and order flow");
    Ok((order_1, order_2))
}

async fn phase_2(app: &App) -> Result<(), AnyError> {
    let before_b = generation(app, "venue-b-feed");
    let venues = app.handle.supervisor("venues").expect("venues supervisor");
    let restarted = venues.monitor_restart("venue-a-feed")?;
    app.venue_a_feed.send(FeedMsg::Crash).await?;
    tokio::time::timeout(PHASE_TIMEOUT, restarted).await??;
    await_until(|| async {
        status(&app.reconciler).await.is_some_and(|status| {
            status
                .transitions
                .get(VENUE_A)
                .is_some_and(|log| log.contains(&VenueHealth::Down))
        })
    })
    .await?;
    tick(&app.venue_b_feed, VENUE_B, "BTC-USD", 2).await?;
    tick(&app.venue_a_feed, VENUE_A, "BTC-USD", 2).await?;
    await_until(|| async {
        status(&app.reconciler).await.is_some_and(|status| {
            status.venues.get(VENUE_A) == Some(&VenueHealth::Fresh)
                && status.venues.get(VENUE_B) == Some(&VenueHealth::Fresh)
        })
    })
    .await?;
    assert_eq!(generation(app, "venue-b-feed"), before_b);
    assert!(!bounded_call(&app.health, |reply| HealthMsg::Tripped { reply }).await?);
    println!("PHASE 2 OK — isolated panic, Down, and recovery");
    Ok(())
}

async fn phase_3(app: &App) -> Result<(), AnyError> {
    tick(&app.venue_b_feed, VENUE_B, "BTC-USD", 3).await?;
    let keepalive_stop = CancellationToken::new();
    let keepalive = tokio::spawn({
        let feed = app.venue_a_feed.clone();
        let stop = keepalive_stop.clone();
        async move {
            let mut seq = 100;
            while !stop.is_cancelled() {
                let _ = tick(&feed, VENUE_A, "BTC-USD", seq).await;
                seq += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    });
    await_until(|| async {
        status(&app.reconciler).await.is_some_and(|status| {
            status.venues.get(VENUE_A) == Some(&VenueHealth::Fresh)
                && status.venues.get(VENUE_B) == Some(&VenueHealth::Stale)
        })
    })
    .await?;
    keepalive_stop.cancel();
    keepalive.await?;
    tick(&app.venue_b_feed, VENUE_B, "BTC-USD", 4).await?;
    await_until(|| async {
        status(&app.reconciler)
            .await
            .is_some_and(|status| status.venues.get(VENUE_B) == Some(&VenueHealth::Fresh))
    })
    .await?;
    println!("PHASE 3 OK — independent state-timeout staleness");
    Ok(())
}

async fn phase_4(app: &App) -> Result<OrderKey, AnyError> {
    let started = Instant::now();
    let key = expect_unknown(submit(&app.router, "STALL-NOACCEPT", 3, VENUE_A).await?)?;
    assert!(started.elapsed() < router::CALL_DEADLINE * 2);
    await_until(|| async {
        bounded_call(&app.router, |reply| RouterMsg::ReconcileAll { reply })
            .await
            .is_ok_and(|resolved| resolved == 1)
    })
    .await?;
    assert_eq!(app.venue_a.accept_count(&key), 1);
    await_until(|| async { app.venue_a.status(&key) == Some(OrderStatus::Filled) }).await?;
    println!("PHASE 4 OK — stuck request timed out and reconciled");
    Ok(key)
}

async fn phase_5(app: &App) -> Result<OrderKey, AnyError> {
    let key = expect_unknown(submit(&app.router, "ACCEPT-NOACK", 4, VENUE_B).await?)?;
    await_until(|| async {
        bounded_call(&app.router, |reply| RouterMsg::ReconcileAll { reply })
            .await
            .is_ok_and(|resolved| resolved == 1)
    })
    .await?;
    assert_eq!(app.venue_b.accept_count(&key), 1);
    let report = ledger_report(&app.ledger).await.expect("ledger available");
    assert_eq!(
        report
            .effects
            .get(&key)
            .expect("reconciled ledger effect")
            .acknowledgements,
        1
    );
    println!("PHASE 5 OK — unknown accepted order reconciled once");
    Ok(key)
}

async fn phase_6(app: &App) -> Result<(), AnyError> {
    let open = expect_placed(submit(&app.router, "OK", 5, VENUE_A).await?)?;
    let flood_stop = CancellationToken::new();
    let flood = tokio::spawn({
        let a = app.venue_a_feed.clone();
        let b = app.venue_b_feed.clone();
        let stop = flood_stop.clone();
        async move {
            let mut seq = 1_000;
            while !stop.is_cancelled() {
                let now = Instant::now();
                let _ = a
                    .send(FeedMsg::Tick(snapshot(VENUE_A, "BTC-USD", seq, now)))
                    .await;
                let _ = b
                    .send(FeedMsg::Tick(snapshot(VENUE_B, "ETH-USD", seq, now)))
                    .await;
                seq += 1;
            }
        }
    });
    let cancelled = tokio::time::timeout(
        URGENT_BOUND,
        app.control
            .call(|reply| ControlMsg::EmergencyCancelAll { reply }),
    )
    .await??;
    assert!(cancelled >= 1);
    assert_eq!(app.venue_a.status(&open), Some(OrderStatus::Cancelled));
    flood_stop.cancel();
    flood.await?;
    let feed_stats = app.venue_graph.stats();
    for id in ["venue-a-feed", "venue-b-feed"] {
        assert!(
            feed_stats
                .iter()
                .find(|stats| stats.actor_id == id)
                .is_some_and(|stats| stats.messages_conflated > 0),
            "{id} must demonstrate latest-wins conflation"
        );
    }
    println!("PHASE 6 OK — urgent control stayed responsive under saturation");
    Ok(())
}

async fn phase_7(app: &App) -> Result<(), AnyError> {
    app.health.send(HealthMsg::ResetBreaker).await?;
    let venues = app.handle.supervisor("venues").expect("venues supervisor");
    for (id, feed) in [
        ("venue-a-feed", &app.venue_a_feed),
        ("venue-b-feed", &app.venue_b_feed),
        ("venue-a-feed", &app.venue_a_feed),
        ("venue-b-feed", &app.venue_b_feed),
    ] {
        let restarted = venues.monitor_restart(id)?;
        feed.send(FeedMsg::Crash).await?;
        tokio::time::timeout(PHASE_TIMEOUT, restarted).await??;
    }
    await_until(|| async {
        bounded_call(&app.health, |reply| HealthMsg::Tripped { reply })
            .await
            .unwrap_or(false)
    })
    .await?;
    assert_eq!(
        submit(&app.router, "OK", 1, VENUE_A).await?,
        SubmitResult::IntakeClosed
    );
    println!("PHASE 7 OK — aggregate venue restart breaker tripped");
    Ok(())
}

async fn phase_8(
    app: App,
    latency: LatencyRecorder,
    metrics: Snapshotter,
    checked_orders: [&str; 3],
) -> Result<(), AnyError> {
    assert!(!app.intake_gate.load(Ordering::Acquire));
    let _ = bounded_call(&app.router, |reply| RouterMsg::ReconcileAll { reply }).await?;
    await_until(|| async {
        bounded_call(&app.router, |reply| RouterMsg::InFlight { reply })
            .await
            .is_ok_and(|count| count == 0)
    })
    .await?;

    // Sibling shutdown is concurrent. The application stages teardown first:
    // intake is closed, unknown intent is reconciled, and only then are venue
    // and support actors stopped. DrainPolicy alone does not order siblings.
    app.background_stop.cancel();
    app.sampler.await?;
    app.event_pump.await?;
    tokio::time::timeout(Duration::from_secs(5), app.handle.shutdown_and_wait()).await??;

    let latency = latency.snapshot();
    for series in [
        "queue.market",
        "handler.feed",
        "queue.fill",
        "handler.gateway",
    ] {
        assert!(latency.get(series).is_some_and(|sample| sample.count > 0));
    }
    for key in checked_orders {
        assert!(app.venue_a.qty(key).is_some() || app.venue_b.qty(key).is_some());
    }
    println!("latency summary: {latency:#?}");
    let selected_metrics = metrics
        .snapshot()
        .into_vec()
        .into_iter()
        .filter(|(key, _, _, _)| {
            matches!(
                key.key().name(),
                "actor.message.bytes_accepted" | "supervisor.restarts"
            )
        })
        .collect::<Vec<_>>();
    assert!(
        selected_metrics
            .iter()
            .filter(|(key, _, _, _)| key.key().name() == "actor.message.bytes_accepted")
            .count()
            >= 2
    );
    println!("selected metrics: {selected_metrics:#?}");
    println!("final supervisor snapshot: {:#?}", app.handle.snapshot());
    println!("final venue actor stats: {:#?}", app.venue_graph.stats());
    println!("PHASE 8 OK — staged shutdown and observability");
    Ok(())
}

async fn bounded_call<M, T>(
    actor: &ActorRef<M>,
    message: impl FnOnce(Reply<T>) -> M,
) -> Result<T, AnyError>
where
    M: Send + 'static,
    T: Send + 'static,
{
    Ok(tokio::time::timeout(PHASE_TIMEOUT, actor.call(message)).await??)
}

async fn await_until<F, Fut>(mut predicate: F) -> Result<(), AnyError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    tokio::time::timeout(PHASE_TIMEOUT, async {
        loop {
            if predicate().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    Ok(())
}

async fn submit(
    router: &ActorRef<RouterMsg>,
    symbol: &'static str,
    qty: i64,
    venue: VenueId,
) -> Result<SubmitResult, AnyError> {
    bounded_call(router, |reply| RouterMsg::Submit {
        symbol,
        qty,
        venue,
        reply,
    })
    .await
}

async fn status(reconciler: &ActorRef<ReconcilerMsg>) -> Option<ReconcilerStatus> {
    bounded_call(reconciler, |reply| ReconcilerMsg::Status { reply })
        .await
        .ok()
}

async fn ledger_report(ledger: &ActorRef<LedgerMsg>) -> Option<LedgerReport> {
    bounded_call(ledger, |reply| LedgerMsg::Report { reply })
        .await
        .ok()
}

async fn tick(
    feed: &ActorRef<FeedMsg>,
    venue: VenueId,
    symbol: &'static str,
    seq: u64,
) -> Result<(), AnyError> {
    tokio::time::timeout(
        PHASE_TIMEOUT,
        feed.send(FeedMsg::Tick(snapshot(venue, symbol, seq, Instant::now()))),
    )
    .await??;
    Ok(())
}

fn snapshot(
    venue: VenueId,
    symbol: &'static str,
    seq: u64,
    enqueued_at: Instant,
) -> MarketSnapshot {
    MarketSnapshot {
        venue,
        symbol,
        bid: 100_000 + seq as i64,
        ask: 100_010 + seq as i64,
        seq,
        enqueued_at,
    }
}

fn both_health(status: &ReconcilerStatus, health: VenueHealth) -> bool {
    [VENUE_A, VENUE_B]
        .iter()
        .all(|venue| status.venues.get(venue) == Some(&health))
}

fn generation(app: &App, child: &str) -> u64 {
    app.handle
        .snapshot()
        .descendant(["venues", child])
        .expect("nested child snapshot")
        .generation
}

fn expect_placed(result: SubmitResult) -> Result<OrderKey, AnyError> {
    match result {
        SubmitResult::Placed(key) => Ok(key),
        other => Err(format!("expected placed order, got {other:?}").into()),
    }
}

fn expect_unknown(result: SubmitResult) -> Result<OrderKey, AnyError> {
    match result {
        SubmitResult::Unknown(key) => Ok(key),
        other => Err(format!("expected unknown order, got {other:?}").into()),
    }
}
