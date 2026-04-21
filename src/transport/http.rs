//! HTTP MCP transport + `/healthz` readiness probe.
//!
//! Uses rmcp's `streamable_http_server::tower::StreamableHttpService` mounted under `/mcp`.
//! `/healthz` is a trivial plaintext probe suitable for Kubernetes liveness/readiness.
//!
//! TLS is **not** terminated here — production deployments terminate at the ingress.
//! The server binds plain HTTP; see `docs/architecture.md`.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;

use crate::config::HttpConfig;
use crate::handler::{DmMcpHandler, Transport};

#[derive(Clone)]
struct HealthState;

/// Run the HTTP transport, serving MCP under `/mcp` and health under `/healthz`.
///
/// Completes when a shutdown signal is received (SIGINT) and all in-flight requests drain.
pub async fn run(cfg: &HttpConfig) -> Result<()> {
    let addr = cfg.socket_addr();
    tracing::info!(bind = %addr, "dm-mcp: serving MCP over HTTP");

    let cancel = CancellationToken::new();

    // Factory: each new MCP session gets its own handler instance. The handler is cheap to
    // construct; future phases may share more state via Arc inside DmMcpHandler.
    let mcp_service = StreamableHttpService::new(
        || Ok(DmMcpHandler::new(Transport::Http)),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_cancellation_token(cancel.child_token()),
    );

    let app = Router::new()
        .route("/healthz", get(healthz))
        .nest_service("/mcp", mcp_service)
        .with_state(HealthState);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {addr}"))?;

    let shutdown_cancel = cancel.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        match tokio::signal::ctrl_c().await {
            Ok(_) => tracing::info!("shutdown signal received"),
            Err(e) => tracing::warn!(error = %e, "failed to install shutdown handler"),
        }
        shutdown_cancel.cancel();
    });

    server.await.context("HTTP server failed")?;
    Ok(())
}

/// Readiness/liveness probe. Phase 1 returns 200 unconditionally — later phases will verify
/// the SQLite connection is reachable before reporting healthy.
async fn healthz(State(_): State<HealthState>) -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}
