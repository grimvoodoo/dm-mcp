//! E2E test for Phase 6 — full setup flow walks new → 3 answers → generate → ready and
//! asserts the resulting DB state matches the Roadmap requirements:
//!
//! - Starting zone exists with biome matching the `starting_biome` answer.
//! - 2-5 stub neighbour zone rows.
//! - `campaign_state.phase = 'running'`.
//! - `campaign.started` event recorded at `campaign_hour = 0`.

use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::process::Command;

fn bin_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_dm-mcp"))
}

struct Harness {
    _tmp: TempDir,
    db_path: std::path::PathBuf,
    client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
}

async fn connect() -> Result<Harness> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("campaign.db");
    let mut cmd = Command::new(bin_path());
    cmd.arg("stdio");
    cmd.env("DMMCP_LOG_LEVEL", "warn");
    cmd.env("DMMCP_DB_PATH", &db_path);
    let transport = TokioChildProcess::new(cmd).context("spawn child")?;
    let client = ().serve(transport).await.context("mcp handshake")?;
    Ok(Harness {
        _tmp: tmp,
        db_path,
        client,
    })
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let obj = args.as_object().cloned().unwrap_or_default();
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(obj);
    let result = client
        .call_tool(params)
        .await
        .with_context(|| format!("call {name}"))?;
    assert!(
        result.is_error != Some(true),
        "{name} signalled error: {:?}",
        result
    );
    let text = result
        .content
        .iter()
        .find_map(|item| match &item.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .context("expected text content")?;
    serde_json::from_str(&text).with_context(|| format!("{name} payload: {text}"))
}

#[tokio::test]
async fn full_setup_flow_lands_in_running_with_world_in_place() -> Result<()> {
    let h = connect().await?;

    // 1. new_campaign — should be in setup phase, returning the questions.
    let new = call(&h.client, "setup.new_campaign", serde_json::json!({})).await?;
    assert_eq!(new["phase"].as_str(), Some("setup"));
    let qs = new["questions"].as_array().context("questions array")?;
    let q_ids: Vec<&str> = qs.iter().filter_map(|q| q["id"].as_str()).collect();
    for required in ["starting_biome", "enemy_preference", "tone"] {
        assert!(
            q_ids.contains(&required),
            "expected question {required} in setup; got {q_ids:?}"
        );
    }

    // 2. answer all three.
    call(
        &h.client,
        "setup.answer",
        serde_json::json!({
            "question_id": "starting_biome",
            "answer": "temperate_forest"
        }),
    )
    .await?;
    call(
        &h.client,
        "setup.answer",
        serde_json::json!({
            "question_id": "enemy_preference",
            "answer": ["humanoid_raiders", "beasts"]
        }),
    )
    .await?;
    call(
        &h.client,
        "setup.answer",
        serde_json::json!({
            "question_id": "tone",
            "answer": "balanced"
        }),
    )
    .await?;

    // 3. generate_world — starting zone + 2-5 neighbours.
    let gw = call(&h.client, "setup.generate_world", serde_json::json!({})).await?;
    assert_eq!(
        gw["starting_biome"].as_str(),
        Some("temperate_forest"),
        "starting zone biome must match starting_biome answer"
    );
    let neighbours = gw["neighbour_zone_ids"]
        .as_array()
        .context("neighbour_zone_ids array")?;
    assert!(
        (2..=5).contains(&neighbours.len()),
        "expected 2-5 neighbours, got {}",
        neighbours.len()
    );
    let starting_zone_id = gw["starting_zone_id"].as_i64().unwrap();

    // 4. mark_ready — phase flips to 'running'.
    let mr = call(&h.client, "setup.mark_ready", serde_json::json!({})).await?;
    assert_eq!(mr["phase"].as_str(), Some("running"));

    h.client.cancel().await?;

    // 5. Inspect the DB directly: campaign_state, zones, events.
    let conn = Connection::open_with_flags(
        &h.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    let phase: String =
        conn.query_row("SELECT phase FROM campaign_state WHERE id = 1", [], |row| {
            row.get(0)
        })?;
    assert_eq!(phase, "running");

    let starting_biome: String = conn.query_row(
        "SELECT biome FROM zones WHERE id = ?1",
        [starting_zone_id],
        |row| row.get(0),
    )?;
    assert_eq!(starting_biome, "temperate_forest");

    let neighbour_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM zones WHERE id != ?1",
        [starting_zone_id],
        |row| row.get(0),
    )?;
    assert!(
        (2..=5).contains(&neighbour_count),
        "expected 2-5 neighbour zone rows, got {neighbour_count}"
    );

    // Bidirectional connections — every neighbour has both forward and reverse edges
    // from/to the starting zone.
    let conn_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM zone_connections
         WHERE from_zone_id = ?1 OR to_zone_id = ?1",
        [starting_zone_id],
        |row| row.get(0),
    )?;
    assert_eq!(
        conn_count,
        neighbour_count * 2,
        "expected {} connections (2 per neighbour), got {conn_count}",
        neighbour_count * 2
    );

    // campaign.started event recorded at campaign_hour = 0.
    let (started_kind, started_hour): (String, i64) = conn.query_row(
        "SELECT kind, campaign_hour FROM events WHERE kind = 'campaign.started'
         ORDER BY id DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(started_kind, "campaign.started");
    assert_eq!(
        started_hour, 0,
        "campaign.started should be at campaign_hour = 0"
    );

    Ok(())
}

#[tokio::test]
async fn setup_tools_are_listed() -> Result<()> {
    let h = connect().await?;
    let tools = h.client.list_all_tools().await?;
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for required in [
        "setup.new_campaign",
        "setup.answer",
        "setup.generate_world",
        "setup.mark_ready",
    ] {
        assert!(
            names.contains(&required),
            "missing tool {required}; got {names:?}"
        );
    }
    h.client.cancel().await?;
    Ok(())
}
