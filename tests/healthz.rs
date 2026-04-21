//! E2E test: `GET /healthz` returns 200 when the HTTP transport is running.
//!
//! Spawns the compiled binary in a subprocess, waits for the listener to come up, hits
//! `/healthz` with a real HTTP client, asserts 200, then kills the child.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tokio::time::sleep;

/// Pick a port that's unlikely to collide with anything else on the machine. Using
/// `bind("127.0.0.1:0")` to have the kernel assign one would be cleaner, but that would require
/// the binary to report its bound port back — Phase 1 doesn't have that mechanism. A
/// process-id-derived offset is enough for a single test binary.
fn test_port() -> u16 {
    // Stay well clear of ephemeral ports; Linux's default ip_local_port_range starts at 32768.
    const BASE: u16 = 18_000;
    (BASE + (std::process::id() as u16 % 1_000)).max(BASE)
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

async fn spawn_http(port: u16) -> anyhow::Result<Child> {
    let child = Command::new(bin_path())
        .arg("http")
        .env("DMMCP_HTTP_BIND", "127.0.0.1")
        .env("DMMCP_HTTP_PORT", port.to_string())
        .env("DMMCP_LOG_LEVEL", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    Ok(child)
}

#[tokio::test]
async fn healthz_returns_ok_when_http_transport_is_running() -> anyhow::Result<()> {
    let port = test_port();
    let mut child = spawn_http(port).await?;

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
    let port = test_port() + 1;
    let mut child = spawn_http(port).await?;

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
