//! Conformance signaling server library.
//!
//! Serves the HTTP mailbox protocol in `conformance/signaling/PROTOCOL.md` on
//! top of the in-memory [`state::Rooms`] store. Exposed as a library so both the
//! `conformance-signalingd` binary and the conformance runner can start and stop
//! a server in-process.

pub mod state;

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use state::{Fetch, Limits, Publish, Role, Rooms};

/// Server configuration.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Default long-poll upper bound when a request omits `wait`.
    pub long_poll: Duration,
    /// Maximum accepted publish body size, in bytes.
    pub max_blob_bytes: usize,
    /// Store limits (room TTL, capacity caps).
    pub limits: Limits,
    /// Interval at which expired rooms are swept.
    pub eviction_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            long_poll: Duration::from_millis(25_000),
            max_blob_bytes: 262_144,
            limits: Limits::default(),
            eviction_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Clone)]
struct AppState {
    rooms: Rooms,
    config: Config,
}

/// A running server with the address it bound and a handle to shut it down.
pub struct RunningServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    /// The bound socket address (use to build the base URL).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The base URL clients should use, e.g. `http://127.0.0.1:PORT`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Signal shutdown and wait for the server task to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

fn router_with(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/rooms/{room}/{role}", post(publish))
        .route("/rooms/{room}/{role}", get(fetch))
        .route("/rooms/{room}/{role}/done", post(mark_done))
        .route("/rooms/{room}", delete(delete_room))
        .with_state(state)
}

/// Build a standalone axum router with a fresh store (no background eviction).
/// Primarily for in-process tests that drive the router directly.
pub fn router(config: Config) -> Router {
    router_with(AppState {
        rooms: Rooms::new(config.limits),
        config,
    })
}

/// Bind `addr` and serve until [`RunningServer::shutdown`] is called. Passing a
/// port of 0 binds an ephemeral localhost port; read it back with
/// [`RunningServer::addr`]. A background task periodically evicts rooms whose
/// TTL has elapsed.
pub async fn spawn(addr: SocketAddr, config: Config) -> Result<RunningServer> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding conformance-signalingd to {addr}"))?;
    let bound = listener.local_addr()?;

    let rooms = Rooms::new(config.limits);
    let app = router_with(AppState {
        rooms: rooms.clone(),
        config,
    });

    let (tx, rx) = oneshot::channel::<()>();

    // Background TTL eviction, stopped when the shutdown signal fires.
    let evict_rooms = rooms.clone();
    let evict_interval = config.eviction_interval;
    let (evict_stop_tx, mut evict_stop_rx) = oneshot::channel::<()>();
    let evict_join = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(evict_interval);
        loop {
            tokio::select! {
                _ = ticker.tick() => { evict_rooms.evict_expired().await; }
                _ = &mut evict_stop_rx => break,
            }
        }
    });

    let join = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = rx.await;
        });
        if let Err(e) = server.await {
            tracing::error!(target: "signalingd", error = %e, "serve error");
        }
        let _ = evict_stop_tx.send(());
        let _ = evict_join.await;
    });

    Ok(RunningServer {
        addr: bound,
        shutdown: Some(tx),
        join,
    })
}

// ----- handlers -------------------------------------------------------------

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

fn valid_room(room: &str) -> bool {
    !room.is_empty()
        && room.len() <= 128
        && room
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

async fn publish(
    State(app): State<AppState>,
    Path((room, role)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if !valid_room(&room) {
        return error(StatusCode::BAD_REQUEST, "bad-room", None);
    }
    let role = match Role::parse(&role) {
        Some(r) => r,
        None => return error(StatusCode::BAD_REQUEST, "bad-role", None),
    };
    if body.len() > app.config.max_blob_bytes {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "too-large", "limit": app.config.max_blob_bytes })),
        )
            .into_response();
    }

    match app.rooms.publish(&room, role, body).await {
        Publish::Seq(seq) => {
            tracing::debug!(target: "signalingd", %room, ?role, seq, "publish");
            (StatusCode::OK, Json(json!({ "seq": seq }))).into_response()
        }
        Publish::Done => {
            tracing::warn!(target: "signalingd", %room, ?role, "publish to done mailbox");
            error(StatusCode::CONFLICT, "done", None)
        }
        Publish::Capacity => {
            tracing::warn!(target: "signalingd", %room, ?role, "publish over capacity");
            error(StatusCode::TOO_MANY_REQUESTS, "capacity", None)
        }
    }
}

#[derive(Debug, Deserialize)]
struct FetchQuery {
    seq: Option<String>,
    wait: Option<u64>,
}

async fn fetch(
    State(app): State<AppState>,
    Path((room, role)): Path<(String, String)>,
    Query(q): Query<FetchQuery>,
) -> Response {
    if !valid_room(&room) {
        return error(StatusCode::BAD_REQUEST, "bad-room", None);
    }
    let role = match Role::parse(&role) {
        Some(r) => r,
        None => return error(StatusCode::BAD_REQUEST, "bad-role", None),
    };
    let seq: usize = match q.seq.as_deref().map(str::parse) {
        Some(Ok(n)) => n,
        _ => return error(StatusCode::BAD_REQUEST, "bad-seq", None),
    };
    let wait = match q.wait {
        Some(ms) => Duration::from_millis(ms),
        None => app.config.long_poll,
    };

    match app.rooms.fetch(&room, role, seq, wait).await {
        Fetch::Blob(bytes) => {
            tracing::debug!(target: "signalingd", %room, ?role, seq, len = bytes.len(), "fetch: blob");
            let mut headers = HeaderMap::new();
            headers.insert("x-seq", seq.to_string().parse().unwrap());
            headers.insert(
                header::CONTENT_TYPE,
                "application/octet-stream".parse().unwrap(),
            );
            (StatusCode::OK, headers, bytes).into_response()
        }
        Fetch::Done => {
            tracing::debug!(target: "signalingd", %room, ?role, seq, "fetch: done");
            let mut headers = HeaderMap::new();
            headers.insert("x-done", "true".parse().unwrap());
            (StatusCode::NO_CONTENT, headers).into_response()
        }
        Fetch::Pending => {
            tracing::debug!(target: "signalingd", %room, ?role, seq, "fetch: pending (long-poll expired)");
            let mut headers = HeaderMap::new();
            headers.insert("x-seq", seq.to_string().parse().unwrap());
            (StatusCode::NOT_MODIFIED, headers).into_response()
        }
    }
}

async fn mark_done(
    State(app): State<AppState>,
    Path((room, role)): Path<(String, String)>,
    // Accepting (and thereby draining) the request body matters even though the
    // protocol sends none: a client that streams an empty body (e.g. chunked
    // with just a terminator) races a handler that responds without reading it
    // — body data arriving after the server side closes elicits a TCP RST that
    // can destroy the client's buffered response.
    _body: Bytes,
) -> Response {
    if !valid_room(&room) {
        return error(StatusCode::BAD_REQUEST, "bad-room", None);
    }
    let role = match Role::parse(&role) {
        Some(r) => r,
        None => return error(StatusCode::BAD_REQUEST, "bad-role", None),
    };
    let len = app.rooms.mark_done(&room, role).await;
    tracing::debug!(target: "signalingd", %room, ?role, len, "mark done");
    (StatusCode::OK, Json(json!({ "done": true, "len": len }))).into_response()
}

async fn delete_room(State(app): State<AppState>, Path(room): Path<String>) -> Response {
    if !valid_room(&room) {
        return error(StatusCode::BAD_REQUEST, "bad-room", None);
    }
    let deleted = app.rooms.delete(&room).await;
    (StatusCode::OK, Json(json!({ "deleted": deleted }))).into_response()
}

fn error(status: StatusCode, code: &str, detail: Option<&str>) -> Response {
    let body = match detail {
        Some(d) => json!({ "error": code, "detail": d }),
        None => json!({ "error": code }),
    };
    (status, Json(body)).into_response()
}
