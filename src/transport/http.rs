//! HTTP MCP transport + `/healthz` readiness probe.
//!
//! Uses rmcp's `streamable_http_server::tower::StreamableHttpService` mounted under `/mcp`.
//! `/healthz` is a plaintext probe suitable for Kubernetes liveness/readiness; runs a
//! cheap `SELECT 1` against the DB so a stuck-mutex / corrupt-DB instance reports
//! unhealthy instead of accepting traffic.
//!
//! Hardening (#28):
//! - default bind is `127.0.0.1` (single-tenant per CLAUDE.md); operators that need
//!   network exposure set `DMMCP_HTTP_BIND=0.0.0.0` explicitly.
//! - body size capped at `DMMCP_HTTP_MAX_BODY_BYTES` (default 1 MiB) via
//!   `axum::extract::DefaultBodyLimit`.
//! - optional bearer token via `DMMCP_HTTP_AUTH_TOKEN`. When set, requests to `/mcp`
//!   without `Authorization: Bearer <token>` get 401. Constant-time compared.
//!
//! TLS is **not** terminated here — production deployments terminate at the ingress.
//! The server binds plain HTTP; see `docs/architecture.md`.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;

use crate::config::HttpConfig;
use crate::content::Content;
use crate::db::DbHandle;
use crate::handler::{DmMcpHandler, Transport};

#[derive(Clone)]
struct AppState {
    db: DbHandle,
    /// `Some(token)` enables bearer-token auth on `/mcp`. `None` means no auth — only
    /// safe for loopback or trusted-network deployments.
    auth_token: Option<Arc<String>>,
}

/// Run the HTTP transport, serving MCP under `/mcp` and health under `/healthz`.
///
/// Completes when a shutdown signal is received (SIGINT / SIGTERM) and all in-flight
/// requests drain.
pub async fn run(cfg: &HttpConfig, content: Arc<Content>, db: DbHandle) -> Result<()> {
    let addr = cfg.socket_addr();
    let auth_enabled = cfg.auth_token.is_some();
    tracing::info!(
        bind = %addr,
        auth = auth_enabled,
        max_body_bytes = cfg.max_body_bytes,
        "dm-mcp: serving MCP over HTTP"
    );

    let cancel = CancellationToken::new();

    // Factory: each new MCP session gets its own handler instance, sharing the same
    // content catalog and DB handle via Arc clone.
    let mcp_service = StreamableHttpService::new(
        {
            let content = Arc::clone(&content);
            let db = Arc::clone(&db);
            move || {
                Ok(DmMcpHandler::new(
                    Transport::Http,
                    Arc::clone(&content),
                    Arc::clone(&db),
                ))
            }
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_cancellation_token(cancel.child_token()),
    );

    let state = AppState {
        db: Arc::clone(&db),
        auth_token: cfg.auth_token.clone().map(Arc::new),
    };

    // Mount `/mcp` with bearer-token middleware (no-op when auth_token is None) plus the
    // body-limit layer. `/healthz` is unauthenticated by design — k8s probes need to
    // reach it without a token.
    let mcp_router = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer_token,
        ))
        .layer(DefaultBodyLimit::max(cfg.max_body_bytes));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .merge(mcp_router)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {addr}"))?;

    let shutdown_cancel = cancel.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        wait_for_shutdown().await;
        shutdown_cancel.cancel();
    });

    server.await.context("HTTP server failed")?;
    Ok(())
}

/// Readiness/liveness probe. Acquires the DB mutex and runs `SELECT 1` so a stuck or
/// poisoned mutex / corrupt DB reports 503 instead of falsely advertising healthy.
/// k8s readinessProbe will then steer traffic away.
async fn healthz(State(state): State<AppState>) -> Response {
    // Run the DB probe on a blocking pool — SQLite calls are sync. The probe itself is
    // microseconds; the lock-acquire is what we're really testing.
    let db = Arc::clone(&state.db);
    let outcome = tokio::task::spawn_blocking(move || {
        let conn = db
            .try_lock()
            .map_err(|_| "db mutex poisoned or held by long-running tx")?;
        let one: i64 = conn
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(|e| {
                // Boxed-and-Debug-stringified to keep the closure sync + Send-able.
                Box::leak(format!("SELECT 1 failed: {e}").into_boxed_str()) as &str
            })?;
        if one == 1 {
            Ok::<&'static str, &'static str>("ok")
        } else {
            Err("SELECT 1 returned non-1")
        }
    })
    .await;

    match outcome {
        Ok(Ok(body)) => (StatusCode::OK, body).into_response(),
        Ok(Err(reason)) => {
            tracing::warn!(reason = %reason, "/healthz reporting unhealthy");
            (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "/healthz blocking task panicked");
            (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response()
        }
    }
}

/// Enforce `Authorization: Bearer <token>` if `state.auth_token` is set. No-op when
/// it's `None` (default — preserves existing un-authed behaviour for callers that
/// haven't set the env var). Constant-time compare on the token to defend against
/// timing oracles.
async fn require_bearer_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = &state.auth_token else {
        return next.run(req).await;
    };

    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));

    match provided {
        Some(provided) if constant_time_eq(provided.as_bytes(), expected.as_bytes()) => {
            next.run(req).await
        }
        _ => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response(),
    }
}

/// Constant-time byte-slice equality. Defends against timing oracles when comparing
/// secrets like the bearer token. Length difference is also constant-time-checked.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Wait for a shutdown signal. On Unix hosts we listen for both SIGINT (local development /
/// Ctrl-C) and SIGTERM (Kubernetes pod termination) so `terminationGracePeriodSeconds` is
/// respected and in-flight MCP sessions are cancelled via the shared `CancellationToken`
/// instead of lingering until SIGKILL.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                // Falling back to ctrl_c is strictly worse for K8s, but never installing a
                // handler is worse still — we'd block the task forever.
                tracing::warn!(error = %e, "failed to install SIGTERM handler; falling back to SIGINT only");
                if let Err(e) = tokio::signal::ctrl_c().await {
                    tracing::warn!(error = %e, "failed to await SIGINT");
                }
                return;
            }
        };
        tokio::select! {
            r = tokio::signal::ctrl_c() => {
                match r {
                    Ok(()) => tracing::info!("SIGINT received; starting graceful shutdown"),
                    Err(e) => tracing::warn!(error = %e, "SIGINT handler errored; starting shutdown anyway"),
                }
            }
            _ = term.recv() => {
                tracing::info!("SIGTERM received; starting graceful shutdown");
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "failed to await ctrl_c");
            return;
        }
        tracing::info!("shutdown signal received; starting graceful shutdown");
    }
}
