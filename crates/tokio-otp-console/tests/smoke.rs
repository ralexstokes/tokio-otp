use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{broadcast, watch},
    time::{sleep, timeout},
};
use tokio_otp_console::{ActorStatsView, Console, ConsoleHandle};
use tokio_supervisor::{
    ChildMembershipView, ChildSnapshot, ChildStateView, Strategy, SupervisorEvent,
    SupervisorSnapshot, SupervisorStateView,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type TestWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn snapshot(child_state: ChildStateView) -> SupervisorSnapshot {
    SupervisorSnapshot {
        state: SupervisorStateView::Running,
        strategy: Strategy::OneForOne,
        children: vec![ChildSnapshot {
            id: "worker".into(),
            generation: 0,
            started: child_state == ChildStateView::Running,
            startup_aborted: false,
            state: child_state,
            membership: ChildMembershipView::Active,
            last_exit: None,
            restart_count: 0,
            next_restart_in: None,
            supervisor: None,
        }],
    }
}

fn actor_stats() -> Vec<ActorStatsView> {
    vec![ActorStatsView {
        actor_id: "worker".into(),
        messages_received: 11,
        messages_accepted: 10,
        messages_conflated: 3,
        message_bytes_accepted: None,
        sends_rejected: 1,
        mailbox_depth: 3,
        mailbox_capacity: 32,
    }]
}

async fn spawn_console_with_stats(
    stats: impl Fn() -> Vec<ActorStatsView> + Send + Sync + 'static,
) -> (
    ConsoleHandle,
    watch::Sender<SupervisorSnapshot>,
    broadcast::Sender<SupervisorEvent>,
) {
    let (snapshot_tx, snapshot_rx) = watch::channel(snapshot(ChildStateView::Running));
    let (event_tx, _) = broadcast::channel(16);
    let handle = Console::builder()
        .snapshots(snapshot_rx)
        .events(event_tx.clone())
        .actor_stats(stats)
        .bind(([127, 0, 0, 1], 0))
        .build()
        .spawn()
        .await
        .expect("failed to spawn console");

    (handle, snapshot_tx, event_tx)
}

async fn spawn_console() -> (
    ConsoleHandle,
    watch::Sender<SupervisorSnapshot>,
    broadcast::Sender<SupervisorEvent>,
) {
    spawn_console_with_stats(actor_stats).await
}

async fn connect(addr: SocketAddr) -> TestWebSocket {
    connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("failed to connect websocket")
        .0
}

async fn read_json(socket: &mut TestWebSocket) -> Value {
    let message = timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out waiting for websocket frame")
        .expect("websocket closed before the expected frame")
        .expect("failed to read websocket frame");
    let Message::Text(text) = message else {
        panic!("expected a text websocket frame, got {message:?}");
    };

    serde_json::from_str(&text).expect("websocket frame was not valid JSON")
}

async fn read_non_stats_json(socket: &mut TestWebSocket) -> Value {
    loop {
        let frame = read_json(socket).await;
        if frame["type"] != "actor_stats" {
            return frame;
        }
    }
}

async fn read_handshake(socket: &mut TestWebSocket) {
    let snapshot = read_json(socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    let stats = read_json(socket).await;
    assert_eq!(stats["type"], "actor_stats");
}

#[tokio::test]
async fn index_serves_dashboard() {
    let (handle, _snapshot_tx, _event_tx) = spawn_console().await;
    let mut stream = TcpStream::connect(handle.local_addr())
        .await
        .expect("failed to connect to console");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("failed to send HTTP request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("failed to read HTTP response");
    let response = String::from_utf8(response).expect("HTTP response was not UTF-8");
    assert!(response.starts_with("HTTP/1.1 200"));
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response did not contain a header/body separator");
    assert!(headers.to_ascii_lowercase().contains("text/html"));
    assert!(body.contains("tokio-otp console"));
}

#[tokio::test]
async fn ws_sends_snapshot_then_stats_on_connect() {
    let (handle, _snapshot_tx, _event_tx) = spawn_console().await;
    let mut socket = connect(handle.local_addr()).await;

    let snapshot = read_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["data"]["children"][0]["id"], "worker");

    let stats = read_json(&mut socket).await;
    assert_eq!(stats["type"], "actor_stats");
    assert_eq!(
        stats["data"],
        json!([{
            "actor_id": "worker",
            "messages_received": 11,
            "messages_accepted": 10,
            "messages_conflated": 3,
            "sends_rejected": 1,
            "mailbox_depth": 3,
            "mailbox_capacity": 32,
        }])
    );
}

#[tokio::test]
async fn ws_skips_unchanged_stats() {
    let stats = Arc::new(Mutex::new(actor_stats()));
    let stats_source = Arc::clone(&stats);
    let (handle, _snapshot_tx, _event_tx) = spawn_console_with_stats(move || {
        stats_source
            .lock()
            .expect("actor stats mutex poisoned")
            .clone()
    })
    .await;
    let mut socket = connect(handle.local_addr()).await;
    read_handshake(&mut socket).await;

    let unchanged = timeout(Duration::from_millis(2500), socket.next()).await;
    assert!(
        unchanged.is_err(),
        "received an unexpected frame for unchanged actor stats: {unchanged:?}"
    );

    stats
        .lock()
        .expect("actor stats mutex poisoned")
        .first_mut()
        .expect("actor stats fixture was empty")
        .mailbox_depth += 1;

    let frame = read_json(&mut socket).await;
    assert_eq!(frame["type"], "actor_stats");
    assert_eq!(frame["data"][0]["mailbox_depth"], 4);
}

#[tokio::test]
async fn ws_streams_snapshot_updates() {
    let (handle, snapshot_tx, _event_tx) = spawn_console().await;
    let mut socket = connect(handle.local_addr()).await;
    read_handshake(&mut socket).await;

    snapshot_tx
        .send(snapshot(ChildStateView::Stopped))
        .expect("failed to send snapshot update");

    let frame = read_non_stats_json(&mut socket).await;
    assert_eq!(frame["type"], "snapshot");
    assert_eq!(frame["data"]["children"][0]["state"], "Stopped");
}

#[tokio::test]
async fn ws_streams_events() {
    let (handle, _snapshot_tx, event_tx) = spawn_console().await;
    let mut socket = connect(handle.local_addr()).await;
    read_handshake(&mut socket).await;

    event_tx
        .send(SupervisorEvent::ChildStarted {
            id: "worker".into(),
            generation: 1,
        })
        .expect("failed to broadcast supervisor event");

    let frame = read_non_stats_json(&mut socket).await;
    assert_eq!(frame["type"], "event");
    assert_eq!(
        frame["data"],
        json!({
            "ChildStarted": {
                "id": "worker",
                "generation": 1,
            }
        })
    );
}

#[tokio::test]
async fn shutdown_stops_server() {
    let (handle, _snapshot_tx, _event_tx) = spawn_console().await;
    let addr = handle.local_addr();
    handle.shutdown();

    timeout(Duration::from_secs(2), async {
        while let Ok(stream) = TcpStream::connect(addr).await {
            drop(stream);
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("console still accepted TCP connections after shutdown");
}
