//! E2E test for Phase 2: spawning the binary against a fresh DB path creates the file
//! with every expected table.
//!
//! The binary is launched in HTTP mode (any transport would do; HTTP is used here because
//! `/healthz` gives us a synchronous "you can stop now" signal). We hit `/healthz`, verify
//! the DB file was created, open it read-only, and confirm the schema.

use std::process::Stdio;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::time::sleep;

/// Tables the schema must create. Kept in sync with `src/db/schema.rs::EXPECTED_TABLES` —
/// integration tests shouldn't reach into crate internals, so the list is duplicated here
/// deliberately.
const EXPECTED_TABLES: &[&str] = &[
    "parties",
    "characters",
    "character_proficiencies",
    "character_resources",
    "character_conditions",
    "effects",
    "items",
    "item_enchantments",
    "zones",
    "zone_connections",
    "landmarks",
    "character_zone_knowledge",
    "character_landmark_knowledge",
    "encounters",
    "encounter_participants",
    "events",
    "event_participants",
    "event_items",
    "campaign_state",
    "campaign_setup_answers",
];

fn bin_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_dm-mcp"))
}

fn test_port() -> u16 {
    // Shifted to 19_500..20_499 so the PID-derived port can never collide with
    // tests/healthz.rs (18_000..18_999) regardless of how the PID values line up.
    // Ideally the binary would bind 127.0.0.1:0 and report its port back; that's a
    // follow-up when we gain a "report-bound-port" signal on the HTTP transport.
    const BASE: u16 = 19_500;
    BASE + (std::process::id() as u16 % 1_000)
}

async fn wait_for_healthz(url: &str, timeout: Duration) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(url).send().await {
            if resp.status() == 200 {
                return Ok(());
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow::anyhow!("timed out waiting for {url}"))
}

async fn spawn_http(port: u16, db_path: &std::path::Path) -> anyhow::Result<Child> {
    let child = Command::new(bin_path())
        .arg("http")
        .env("DMMCP_HTTP_BIND", "127.0.0.1")
        .env("DMMCP_HTTP_PORT", port.to_string())
        .env("DMMCP_LOG_LEVEL", "warn")
        .env("DMMCP_DB_PATH", db_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    Ok(child)
}

#[tokio::test]
async fn fresh_start_creates_db_with_every_expected_table() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("campaign.db");
    assert!(!db_path.exists(), "precondition: db should not exist yet");

    let port = test_port();
    let mut child = spawn_http(port, &db_path).await?;
    wait_for_healthz(
        &format!("http://127.0.0.1:{port}/healthz"),
        Duration::from_secs(10),
    )
    .await?;

    assert!(
        db_path.exists(),
        "db file should be created by the server on startup"
    );

    // Open read-only to inspect the schema the server wrote.
    let conn = Connection::open(&db_path)?;
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    for expected in EXPECTED_TABLES {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing table {expected}; got {tables:?}"
        );
    }

    // The campaign singleton row must exist and be in 'setup'.
    let phase: String =
        conn.query_row("SELECT phase FROM campaign_state WHERE id = 1", [], |r| {
            r.get(0)
        })?;
    assert_eq!(phase, "setup");

    child.kill().await?;
    Ok(())
}
