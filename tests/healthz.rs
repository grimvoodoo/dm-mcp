//! E2E test: `GET /healthz` returns 200 when the HTTP transport is running.
//!
//! Spawns the compiled binary in a subprocess, waits for the listener to come up, hits
//! `/healthz` with a real HTTP client, asserts 200, then kills the child.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::time::sleep;

/// Pick a port that's unlikely to collide with anything else on the machine. Using
/// `bind("127.0.0.1:0")` to have the kernel assign one would be cleaner, but that would require
/// the binary to report its bound port back — Phase 1 doesn't have that mechanism. A
/// process-id-derived offset is enough for a single test binary.
fn test_port() -> u16 {
    // Stay well clear of ephemeral ports; Linux's default ip_local_port_range starts at 32768.
    const BASE: u16 = 18_000;
    BASE + (std::process::id() as u16 % 1_000)
}

fn bin_path() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is defined for integration tests — no need for assert_cmd here.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_dm-mcp"))
}

async fn wait_for_healthz(url: &str, timeout: Duration) -> anyhow::Result<reqwest::Response> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<reqwest::Error> = None;
    while Instant::now() < deadline {
        match client.get(url).send().await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last_err = Some(e);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(anyhow::anyhow!(
        "timed out waiting for {url}: {:?}",
        last_err
    ))
}

async fn spawn_http(port: u16, db_path: &std::path::Path) -> anyhow::Result<Child> {
    let child = Command::new(bin_path())
        .arg("http")
        .env("DMMCP_HTTP_BIND", "127.0.0.1")
        .env("DMMCP_HTTP_PORT", port.to_string())
        .env("DMMCP_LOG_LEVEL", "warn")
        // Each test gets its own DB file so parallel tests don't race on migration.
        .env("DMMCP_DB_PATH", db_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Discard stderr — the tests don't assert on log output, and a piped-but-undrained
        // stderr would block the child once a later phase's logging fills the pipe (~64 KB).
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    Ok(child)
}

#[tokio::test]
async fn healthz_returns_ok_when_http_transport_is_running() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("campaign.db");
    let port = test_port();
    let mut child = spawn_http(port, &db_path).await?;

    let url = format!("http://127.0.0.1:{port}/healthz");
    let resp = wait_for_healthz(&url, Duration::from_secs(10)).await?;

    assert_eq!(resp.status(), 200, "expected 200 OK, got {}", resp.status());
    let body = resp.text().await?;
    assert_eq!(body, "ok", "expected body 'ok', got {body:?}");

    child.kill().await?;
    Ok(())
}

#[tokio::test]
async fn healthz_returns_ok_on_repeated_calls() -> anyhow::Result<()> {
    // Readiness / liveness probes hit /healthz repeatedly. Make sure we're not holding
    // state between calls that would cause drift.
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("campaign.db");
    let port = test_port() + 1;
    let mut child = spawn_http(port, &db_path).await?;

    let url = format!("http://127.0.0.1:{port}/healthz");
    // First call also serves as the warm-up wait.
    let resp = wait_for_healthz(&url, Duration::from_secs(10)).await?;
    assert_eq!(resp.status(), 200);

    let client = reqwest::Client::new();
    for _ in 0..5 {
        let r = client.get(&url).send().await?;
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await?, "ok");
    }

    child.kill().await?;
    Ok(())
}

// ── Regression tests for #28 (HTTP transport hardening) ────────────────────

async fn spawn_http_with_env(
    port: u16,
    db_path: &std::path::Path,
    extra_env: &[(&str, String)],
) -> anyhow::Result<Child> {
    let mut cmd = Command::new(bin_path());
    cmd.arg("http")
        .env("DMMCP_HTTP_BIND", "127.0.0.1")
        .env("DMMCP_HTTP_PORT", port.to_string())
        .env("DMMCP_LOG_LEVEL", "warn")
        .env("DMMCP_DB_PATH", db_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    Ok(cmd.spawn()?)
}

#[tokio::test]
async fn mcp_endpoint_rejects_unauth_when_token_set() -> anyhow::Result<()> {
    // With DMMCP_HTTP_AUTH_TOKEN set, requests to /mcp must carry the bearer token
    // or get 401. /healthz remains unauthenticated (k8s probes need to reach it).
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("campaign.db");
    let port = test_port() + 100;
    let token = "test-secret-token-do-not-leak";
    let mut child =
        spawn_http_with_env(port, &db_path, &[("DMMCP_HTTP_AUTH_TOKEN", token.into())]).await?;

    // Wait for the server to come up via /healthz (still unauth).
    let healthz = format!("http://127.0.0.1:{port}/healthz");
    wait_for_healthz(&healthz, Duration::from_secs(10)).await?;

    let client = reqwest::Client::new();
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");

    // No token → 401.
    let unauth = client.post(&mcp_url).body("{}").send().await?;
    assert_eq!(unauth.status(), 401, "no-token request must be rejected");

    // Wrong token → 401.
    let wrong = client
        .post(&mcp_url)
        .header("Authorization", "Bearer not-the-token")
        .body("{}")
        .send()
        .await?;
    assert_eq!(wrong.status(), 401, "wrong-token request must be rejected");

    // Right token → not 401 (will be a 4xx for malformed MCP content, but auth passed).
    let authed = client
        .post(&mcp_url)
        .header("Authorization", format!("Bearer {token}"))
        .body("{}")
        .send()
        .await?;
    assert_ne!(
        authed.status(),
        401,
        "valid-token request must pass through auth (got 401)"
    );

    child.kill().await?;
    Ok(())
}

// Note on body-limit testing: `axum::extract::DefaultBodyLimit` only fires when an
// extractor (Bytes / Json / Form) reads the body. rmcp's StreamableHttpService rejects
// requests whose Content-Type/Accept headers don't match its expected MCP shape *before*
// reading the body, so an oversized request with the wrong headers gets a 415/406 from
// rmcp without the body limit ever tripping. The protection is still in place — anyone
// hitting `/mcp` with proper headers + an oversized body will get 413 — but exercising
// it from a test requires standing up a full MCP session, which the existing rmcp
// transport-child-process tests do but don't expose body-control hooks for. Coverage
// gap acknowledged; defer to a future test that drives the full MCP handshake.
