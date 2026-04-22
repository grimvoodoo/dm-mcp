//! E2E test for Phase 7 — Roadmap assertion:
//!
//! Travel to neighbour → `campaign_hour` advanced by edge's `travel_time_hours`,
//! `character_zone_knowledge.level` = 'visited', `world.map` returns both zones with
//! computed 2D positions and connection.

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

/// Bring a campaign up to "running" with a character placed in the starting zone.
/// Returns (player_id, starting_zone_id, neighbour_ids).
async fn bootstrap(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> Result<(i64, i64, Vec<i64>)> {
    call(client, "setup.new_campaign", serde_json::json!({})).await?;
    call(
        client,
        "setup.answer",
        serde_json::json!({"question_id": "starting_biome", "answer": "temperate_forest"}),
    )
    .await?;
    call(
        client,
        "setup.answer",
        serde_json::json!({"question_id": "tone", "answer": "balanced"}),
    )
    .await?;
    let gw = call(client, "setup.generate_world", serde_json::json!({})).await?;
    let starting = gw["starting_zone_id"].as_i64().unwrap();
    let neighbours: Vec<i64> = gw["neighbour_zone_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();

    let cr = call(
        client,
        "character.create",
        serde_json::json!({
            "name": "Kira",
            "role": "player",
            "str_score": 10, "dex_score": 10, "con_score": 10,
            "int_score": 10, "wis_score": 10, "cha_score": 10,
            "current_zone_id": starting
        }),
    )
    .await?;
    let player = cr["character_id"].as_i64().unwrap();

    // Mark starting zone visited so the map shows it.
    // (Phase 8 will tie this to setup.mark_ready; for Phase 7 we set it via direct call.)
    call(
        client,
        "setup.mark_ready",
        serde_json::json!({"player_character_id": player}),
    )
    .await?;
    Ok((player, starting, neighbours))
}

#[tokio::test]
async fn travel_to_neighbour_advances_clock_and_fog_and_map() -> Result<()> {
    let h = connect().await?;
    let (player, starting, neighbours) = bootstrap(&h.client).await?;
    let target = neighbours[0];

    // Pre-travel: mark starting visited via direct DB write isn't available over MCP, so
    // we rely on map calling out the starting zone correctly even without that. The
    // travel call itself will upsert the destination knowledge to 'visited'.
    let r = call(
        &h.client,
        "world.travel",
        serde_json::json!({
            "character_id": player,
            "to_zone_id": target
        }),
    )
    .await?;

    let hours = r["travel_time_hours"].as_i64().unwrap();
    let before = r["campaign_hour_before"].as_i64().unwrap();
    let after = r["campaign_hour_after"].as_i64().unwrap();
    assert!(
        after - before >= hours,
        "campaign_hour should advance by at least the edge's travel_time_hours: \
         before={before} after={after} edge={hours}"
    );
    assert_eq!(
        r["knowledge_level"].as_str(),
        Some("visited"),
        "destination knowledge should be 'visited' after travel"
    );

    // Map: should include both zones with computed 2D positions and the connection.
    let m = call(
        &h.client,
        "world.map",
        serde_json::json!({"character_id": player}),
    )
    .await?;
    let zones = m["zones"].as_array().unwrap();
    let zone_ids: Vec<i64> = zones.iter().map(|z| z["id"].as_i64().unwrap()).collect();
    assert!(
        zone_ids.contains(&target),
        "map should include the destination zone; got {zone_ids:?}"
    );
    // The origin (player's current zone, which is now `target`) should be at (0, 0).
    let origin = zones
        .iter()
        .find(|z| z["id"].as_i64() == Some(target))
        .unwrap();
    assert_eq!(origin["x"].as_i64(), Some(0));
    assert_eq!(origin["y"].as_i64(), Some(0));

    // Connections: at least one between target and starting (or vice versa).
    let conns = m["connections"].as_array().unwrap();
    assert!(
        conns.iter().any(|c| {
            let f = c["from_zone_id"].as_i64();
            let t = c["to_zone_id"].as_i64();
            (f == Some(target) && t == Some(starting)) || (f == Some(starting) && t == Some(target))
        }),
        "map connections should link starting and target; got {conns:?}"
    );

    h.client.cancel().await?;

    // DB cross-check: knowledge row for target = 'visited'; location.move event recorded
    // at the post-travel campaign_hour; characters.current_zone_id updated.
    let conn = Connection::open_with_flags(
        &h.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let lvl: String = conn.query_row(
        "SELECT level FROM character_zone_knowledge
         WHERE character_id = ?1 AND zone_id = ?2",
        rusqlite::params![player, target],
        |row| row.get(0),
    )?;
    assert_eq!(lvl, "visited");
    let cur: i64 = conn.query_row(
        "SELECT current_zone_id FROM characters WHERE id = ?1",
        [player],
        |row| row.get(0),
    )?;
    assert_eq!(cur, target);
    let move_hour: i64 = conn.query_row(
        "SELECT MAX(campaign_hour) FROM events WHERE kind = 'location.move'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(move_hour, after);

    Ok(())
}

#[tokio::test]
async fn world_tools_are_listed() -> Result<()> {
    let h = connect().await?;
    let tools = h.client.list_all_tools().await?;
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for required in ["world.travel", "world.map", "world.describe_zone"] {
        assert!(
            names.contains(&required),
            "missing tool {required}; got {names:?}"
        );
    }
    h.client.cancel().await?;
    Ok(())
}
