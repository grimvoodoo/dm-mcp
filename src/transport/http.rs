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
use crate::content::Content;
use crate::handler::{DmMcpHandler, Transport};

#[derive(Clone)]
struct HealthState;

/// Run the HTTP transport, serving MCP under `/mcp` and health under `/healthz`.
///
/// Completes when a shutdown signal is received (SIGINT / SIGTERM) and all in-flight
/// requests drain.
pub async fn run(cfg: &HttpConfig, content: Arc<Content>) -> Result<()> {
    let addr = cfg.socket_addr();
    tracing::info!(bind = %addr, "dm-mcp: serving MCP over HTTP");

    let cancel = CancellationToken::new();

    // Factory: each new MCP session gets its own handler instance, sharing the same
    // content catalog via Arc clone.
    let mcp_service = StreamableHttpService::new(
        {
            let content = Arc::clone(&content);
            move || Ok(DmMcpHandler::new(Transport::Http, Arc::clone(&content)))
        },
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
        wait_for_shutdown().await;
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
