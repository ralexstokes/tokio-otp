use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use tokio::sync::{broadcast, watch};
use tokio_supervisor::{SupervisorEvent, SupervisorSnapshot};

use crate::{ConsoleHandle, StatsSource, ws};

const INDEX_HTML: &str = include_str!("../assets/index.html");

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) snapshots: watch::Receiver<SupervisorSnapshot>,
    pub(crate) events: broadcast::Sender<SupervisorEvent>,
    pub(crate) stats: StatsSource,
}

#[derive(Clone)]
struct SecurityState {
    allowed_hosts: Arc<HashSet<String>>,
    access_token: Option<Arc<str>>,
}

fn authority_for(addr: SocketAddr) -> String {
    addr.to_string().to_ascii_lowercase()
}

fn host_allowed(headers: &HeaderMap, state: &SecurityState) -> bool {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| state.allowed_hosts.contains(&host.to_ascii_lowercase()))
}

fn token_from_query(request: &Request<Body>) -> Option<&str> {
    request.uri().query()?.split('&').find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name == "token").then_some(value)
    })
}

fn token_from_headers(headers: &HeaderMap) -> Option<&str> {
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        return Some(token);
    }

    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "tokio_otp_console_session").then_some(value)
            })
        })
}

fn token_matches(candidate: Option<&str>, expected: &str) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let length = candidate.len().max(expected.len());
    let difference = (0..length).fold(candidate.len() ^ expected.len(), |difference, index| {
        difference
            | usize::from(
                candidate.as_bytes().get(index).copied().unwrap_or(0)
                    ^ expected.as_bytes().get(index).copied().unwrap_or(0),
            )
    });
    difference == 0
}

async fn security(
    axum::extract::State(state): axum::extract::State<SecurityState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !host_allowed(request.headers(), &state) {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }

    let Some(expected) = state.access_token.as_deref() else {
        return next.run(request).await;
    };

    if request.method() == Method::GET
        && request.uri().path() == "/"
        && token_matches(token_from_query(&request), expected)
    {
        let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
        let cookie =
            format!("tokio_otp_console_session={expected}; HttpOnly; SameSite=Strict; Path=/");
        response.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).expect("validated access token produced invalid cookie"),
        );
        response.headers_mut().insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }

    if !token_matches(token_from_headers(request.headers()), expected) {
        return (StatusCode::UNAUTHORIZED, "valid bearer token required").into_response();
    }

    next.run(request).await
}

fn router(state: AppState, security_state: SecurityState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ws", get(ws::handler))
        .with_state(state)
        .layer(middleware::from_fn_with_state(security_state, security))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn bind_app(
    snapshots: watch::Receiver<SupervisorSnapshot>,
    events: broadcast::Sender<SupervisorEvent>,
    stats: StatsSource,
    bind: SocketAddr,
    access_token: Option<String>,
    allowed_hosts: Vec<String>,
) -> std::io::Result<(tokio::net::TcpListener, Router, SocketAddr)> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    let mut hosts = allowed_hosts
        .into_iter()
        .map(|host| host.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    hosts.insert(authority_for(local_addr));
    if local_addr.ip().is_loopback() {
        hosts.insert(format!("localhost:{}", local_addr.port()));
    }
    let app = router(
        AppState {
            snapshots,
            events,
            stats,
        },
        SecurityState {
            allowed_hosts: Arc::new(hosts),
            access_token: access_token.map(Arc::from),
        },
    );
    Ok((listener, app, local_addr))
}

async fn shutdown_signal(mut shutdown_rx: watch::Receiver<bool>) {
    let _ = shutdown_rx.wait_for(|&shutdown| shutdown).await;
}

pub(crate) async fn spawn(
    snapshots: watch::Receiver<SupervisorSnapshot>,
    events: broadcast::Sender<SupervisorEvent>,
    stats: StatsSource,
    bind: SocketAddr,
    access_token: Option<String>,
    allowed_hosts: Vec<String>,
) -> std::io::Result<ConsoleHandle> {
    let (listener, app, local_addr) =
        bind_app(snapshots, events, stats, bind, access_token, allowed_hosts).await?;
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
    stats: StatsSource,
    bind: SocketAddr,
    access_token: Option<String>,
    allowed_hosts: Vec<String>,
) -> std::io::Result<()> {
    let (listener, app, local_addr) =
        bind_app(snapshots, events, stats, bind, access_token, allowed_hosts).await?;

    tracing::info!(%local_addr, "tokio-otp-console listening");

    axum::serve(listener, app).await
}
