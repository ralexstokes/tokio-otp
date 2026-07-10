use axum::{
    extract::{State, WebSocketUpgrade, ws::Message},
    response::IntoResponse,
};
use tokio::{
    sync::{broadcast, watch},
    time::{self, Duration},
};
use tokio_supervisor::{SupervisorEvent, SupervisorSnapshot};

use crate::{ActorStatsView, server::AppState};

type WebSocket = axum::extract::ws::WebSocket;

pub(crate) async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

fn snapshot_message(snapshot: SupervisorSnapshot) -> Message {
    Message::Text(
        serde_json::json!({ "type": "snapshot", "data": snapshot })
            .to_string()
            .into(),
    )
}

fn event_message(event: SupervisorEvent) -> Message {
    Message::Text(
        serde_json::json!({ "type": "event", "data": event })
            .to_string()
            .into(),
    )
}

fn stats_message(stats: Vec<ActorStatsView>) -> Message {
    Message::Text(
        serde_json::json!({ "type": "actor_stats", "data": stats })
            .to_string()
            .into(),
    )
}

async fn send_snapshot(
    socket: &mut WebSocket,
    snapshots: &mut watch::Receiver<SupervisorSnapshot>,
) -> bool {
    let snapshot = snapshots.borrow_and_update().clone();
    socket.send(snapshot_message(snapshot)).await.is_ok()
}

async fn send_event(socket: &mut WebSocket, event: SupervisorEvent) -> bool {
    socket.send(event_message(event)).await.is_ok()
}

async fn send_stats(socket: &mut WebSocket, state: &AppState) -> bool {
    socket.send(stats_message((state.stats)())).await.is_ok()
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut snapshots = state.snapshots.clone();
    let mut events = state.events.subscribe();

    // Send current snapshot immediately on connect.
    if !send_snapshot(&mut socket, &mut snapshots).await {
        return;
    }
    if !send_stats(&mut socket, &state).await {
        return;
    }

    let mut stats_tick = time::interval(Duration::from_secs(1));
    stats_tick.tick().await;

    loop {
        tokio::select! {
            result = snapshots.changed() => {
                if result.is_err() {
                    break;
                }
                if !send_snapshot(&mut socket, &mut snapshots).await {
                    break;
                }
            }
            result = events.recv() => {
                match result {
                    Ok(event) => {
                        if !send_event(&mut socket, event).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "console websocket event receiver lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            _ = stats_tick.tick() => {
                if !send_stats(&mut socket, &state).await {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}
