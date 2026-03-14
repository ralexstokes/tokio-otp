use std::net::SocketAddr;

use axum::{Router, response::Html, routing::get};
use tokio::sync::{broadcast, watch};
use tokio_supervisor::{SupervisorEvent, SupervisorSnapshot};

use crate::{ConsoleHandle, ws};

const INDEX_HTML: &str = include_str!("../assets/index.html");

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) snapshots: watch::Receiver<SupervisorSnapshot>,
    pub(crate) events: broadcast::Sender<SupervisorEvent>,
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ws", get(ws::handler))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn bind_app(
    snapshots: watch::Receiver<SupervisorSnapshot>,
    events: broadcast::Sender<SupervisorEvent>,
    bind: SocketAddr,
) -> std::io::Result<(tokio::net::TcpListener, Router, SocketAddr)> {
    let app = router(AppState { snapshots, events });
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    Ok((listener, app, local_addr))
}

async fn shutdown_signal(mut shutdown_rx: watch::Receiver<bool>) {
    let _ = shutdown_rx.wait_for(|&shutdown| shutdown).await;
}

pub(crate) async fn spawn(
    snapshots: watch::Receiver<SupervisorSnapshot>,
    events: broadcast::Sender<SupervisorEvent>,
    bind: SocketAddr,
) -> std::io::Result<ConsoleHandle> {
    let (listener, app, local_addr) = bind_app(snapshots, events, bind).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tracing::info!(%local_addr, "tokio-otp-console listening");

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal(shutdown_rx))
            .await
        {
            tracing::warn!(%error, "tokio-otp-console server stopped with error");
        }
    });

    Ok(ConsoleHandle {
        shutdown_tx,
        local_addr,
    })
}

pub(crate) async fn run(
    snapshots: watch::Receiver<SupervisorSnapshot>,
    events: broadcast::Sender<SupervisorEvent>,
    bind: SocketAddr,
) -> std::io::Result<()> {
    let (listener, app, local_addr) = bind_app(snapshots, events, bind).await?;

    tracing::info!(%local_addr, "tokio-otp-console listening");

    axum::serve(listener, app).await
}
