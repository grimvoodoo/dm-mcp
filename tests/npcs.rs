//! E2E tests for Phase 8 (NPC generation + recall).
//!
//! Roadmap assertion:
//!
//!   Generate orc_raider in a zone → character row has stats in archetype range,
//!   proficiencies inserted, loadout items held. Event log has 3–5 `history.backstory`
//!   events with `campaign_hour < 0` and the orc as a participant.
//!   `character.recall(orc.id)` returns those events.

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

/// Run new → starting_biome → generate_world → mark_ready so we have a live zone to drop
/// an NPC into. Returns the starting zone id.
async fn setup_world(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> Result<i64> {
    call(client, "setup.new_campaign", serde_json::json!({})).await?;
    call(
        client,
        "setup.answer",
        serde_json::json!({
            "question_id": "starting_biome",
            "answer": "temperate_forest"
        }),
    )
    .await?;
    let gen = call(client, "setup.generate_world", serde_json::json!({})).await?;
    let starting_zone_id = gen["starting_zone_id"]
        .as_i64()
        .context("starting_zone_id")?;
    call(
        client,
        "setup.mark_ready",
        serde_json::json!({ "player_character_id": null }),
    )
    .await?;
    Ok(starting_zone_id)
}

#[tokio::test]
async fn generate_orc_raider_and_recall_backstory() -> Result<()> {
    let h = connect().await?;
    let zone_id = setup_world(&h.client).await?;

    let gen = call(
        &h.client,
        "npc.generate",
        serde_json::json!({
            "archetype": "orc_raider",
            "zone_id": zone_id
        }),
    )
    .await?;

    let orc_id = gen["character_id"].as_i64().context("character_id")?;
    assert_eq!(gen["role"].as_str(), Some("enemy"));
    assert_eq!(gen["species"].as_str(), Some("orc"));

    let rolled = &gen["rolled_stats"];
    let str_score = rolled["str_score"].as_i64().unwrap();
    let dex_score = rolled["dex_score"].as_i64().unwrap();
    let con_score = rolled["con_score"].as_i64().unwrap();
    assert!(
        (14..=18).contains(&str_score),
        "STR out of orc_raider range 14..=18: {str_score}"
    );
    assert!(
        (10..=14).contains(&dex_score),
        "DEX out of range: {dex_score}"
    );
    assert!(
        (14..=18).contains(&con_score),
        "CON out of range: {con_score}"
    );

    let backstory_ids = gen["backstory_event_ids"]
        .as_array()
        .context("backstory_event_ids")?;
    assert!(
        (3..=5).contains(&backstory_ids.len()),
        "expected 3-5 backstory events, got {}",
        backstory_ids.len()
    );

    // Fetch the character via character.get and verify stats round-trip.
    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": orc_id }),
    )
    .await?;
    assert_eq!(view["str_score"].as_i64(), Some(str_score));
    assert_eq!(
        view["class_or_archetype"].as_str(),
        Some("orc_raider"),
        "archetype should be stored on the character row"
    );
    let profs = view["proficiencies"].as_array().context("proficiencies")?;
    assert!(
        profs
            .iter()
            .any(|p| p["name"] == "greataxe" && p["proficient"].as_bool() == Some(true)),
        "orc_raider should have greataxe proficiency; got {profs:?}"
    );
    assert!(
        profs.iter().any(|p| p["name"] == "intimidation"),
        "orc_raider should have intimidation proficiency"
    );

    // character.recall filtered to history.backstory returns exactly the same set.
    let recalled = call(
        &h.client,
        "character.recall",
        serde_json::json!({
            "character_id": orc_id,
            "kind_prefix": "history."
        }),
    )
    .await?;
    let recalled_events = recalled["events"].as_array().context("events array")?;
    assert_eq!(
        recalled_events.len(),
        backstory_ids.len(),
        "recall should surface every backstory event"
    );
    for ev in recalled_events {
        assert_eq!(ev["kind"].as_str(), Some("history.backstory"));
        assert!(
            ev["campaign_hour"].as_i64().unwrap() < 0,
            "every backstory event should sit at negative campaign_hour"
        );
    }

    // Close before reading the DB directly — see character_core.rs rationale.
    h.client.cancel().await?;

    let conn = Connection::open_with_flags(
        &h.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    // Loadout items: at least the 0.9-chance leather_armor typically lands, plus gold with
    // chance 1.0. Assert gold (the sure thing) is present to keep this deterministic.
    let gold_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM items
             WHERE holder_character_id = ?1 AND base_kind = 'gold'",
            [orc_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        gold_count, 1,
        "orc_raider loadout gold (chance 1.0) should always drop"
    );

    // Every backstory event row has campaign_hour < 0 AND references the orc.
    let bad_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events e
             JOIN event_participants ep ON ep.event_id = e.id
             WHERE ep.character_id = ?1
               AND e.kind = 'history.backstory'
               AND e.campaign_hour >= 0",
            [orc_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bad_events, 0, "no backstory event should be at hour >= 0");

    Ok(())
}

#[tokio::test]
async fn orc_raider_loadout_items_have_real_stats_not_zero() -> Result<()> {
    // Regression for issue #20: npc.generate's loadout path used to insert items with
    // base_kinds that weren't in the content catalog (greataxe, handaxe, leather_armor),
    // which silently materialised as 0-weight, 0-value items. After the fix:
    //  - those bases are authored in content/items/bases/{weapons,armor}.yaml with
    //    SRD-aligned stats,
    //  - Content::validate refuses to load if an archetype loadout names an unknown base.
    //
    // This test asserts the orc_raider's loadout produces items whose effective stats
    // are non-zero — i.e. they're actually backed by real catalog entries.
    let h = connect().await?;
    let zone_id = setup_world(&h.client).await?;

    // Generate enough orc_raiders that the probabilistic loadout draws (chance ≤ 1.0)
    // collectively cover greataxe, handaxe, and leather_armor. With chances 0.7 / 0.5
    // / 0.9 each, 30 rolls gives a vanishing chance of all three sets being empty
    // (roughly 0.3^30 + 0.5^30 + 0.1^30 ≈ effectively zero).
    let mut all_items: Vec<serde_json::Value> = Vec::new();
    for _ in 0..30 {
        let gen = call(
            &h.client,
            "npc.generate",
            serde_json::json!({ "archetype": "orc_raider", "zone_id": zone_id }),
        )
        .await?;
        let orc_id = gen["character_id"].as_i64().unwrap();
        let inv = call(
            &h.client,
            "inventory.get",
            serde_json::json!({ "character_id": orc_id }),
        )
        .await?;
        for item in inv["items"].as_array().unwrap() {
            all_items.push(item.clone());
        }
    }

    // Find at least one of each base kind; assert their effective stats are non-zero.
    for base in ["greataxe", "handaxe", "leather_armor"] {
        let item = all_items
            .iter()
            .find(|i| i["base_kind"] == base)
            .unwrap_or_else(|| {
                panic!(
                    "30 orc_raider rolls produced no {base}; expected at least one given the archetype's chance roll"
                )
            });
        assert!(
            item["effective_weight_lb"].as_f64().unwrap_or(0.0) > 0.0,
            "{base} should have non-zero weight (catalog-backed); got {item:?}"
        );
        assert!(
            item["effective_value_gp"].as_f64().unwrap_or(0.0) > 0.0,
            "{base} should have non-zero value (catalog-backed); got {item:?}"
        );
    }

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn recall_filters_by_kind_prefix_and_since_hour() -> Result<()> {
    let h = connect().await?;
    setup_world(&h.client).await?;

    let gen = call(
        &h.client,
        "npc.generate",
        serde_json::json!({ "archetype": "village_elder" }),
    )
    .await?;
    let char_id = gen["character_id"].as_i64().unwrap();

    // character.recall with no filters returns character.created + history.backstory events.
    let all = call(
        &h.client,
        "character.recall",
        serde_json::json!({ "character_id": char_id }),
    )
    .await?;
    let all_events = all["events"].as_array().unwrap();
    assert!(
        all_events.iter().any(|e| e["kind"] == "character.created"),
        "unfiltered recall should include character.created"
    );
    assert!(
        all_events.iter().any(|e| e["kind"] == "history.backstory"),
        "unfiltered recall should include history.backstory"
    );

    // Filter to non-backstory only by setting since_hour=0.
    let recent = call(
        &h.client,
        "character.recall",
        serde_json::json!({ "character_id": char_id, "since_hour": 0 }),
    )
    .await?;
    let recent_events = recent["events"].as_array().unwrap();
    assert!(
        recent_events
            .iter()
            .all(|e| e["campaign_hour"].as_i64().unwrap() >= 0),
        "since_hour=0 should exclude pre-campaign backstory; got {recent_events:?}"
    );

    h.client.cancel().await?;
    Ok(())
}
