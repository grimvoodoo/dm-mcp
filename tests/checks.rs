//! E2E tests for Phase 5 — the three Roadmap assertions for the check pipeline:
//!
//! 1. Apply Bless (1d4 on persuasion) → resolve persuasion → breakdown contains a rolled
//!    bless die.
//! 2. Apply blinded → resolve attack → two d20s rolled, lower taken.
//! 3. Pass `{kind: ideology_alignment, value: -6}` → event payload records the modifier
//!    with its reason.

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

/// Read the latest event of a given kind by character_id, returning its payload as JSON.
fn latest_event_payload(
    db_path: &std::path::Path,
    character_id: i64,
    kind: &str,
) -> serde_json::Value {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let (payload,): (String,) = conn
        .query_row(
            "SELECT e.payload FROM events e
             JOIN event_participants ep ON ep.event_id = e.id
             WHERE ep.character_id = ?1 AND e.kind = ?2
             ORDER BY e.id DESC LIMIT 1",
            rusqlite::params![character_id, kind],
            |row| Ok((row.get(0)?,)),
        )
        .unwrap_or_else(|e| panic!("no {kind} event for character {character_id}: {e}"));
    serde_json::from_str(&payload).unwrap()
}

async fn make_character(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    name: &str,
) -> Result<i64> {
    let r = call(
        client,
        "character.create",
        serde_json::json!({
            "name": name,
            "role": "player",
            "str_score": 14,
            "dex_score": 12,
            "con_score": 13,
            "int_score": 10,
            "wis_score": 11,
            "cha_score": 14
        }),
    )
    .await?;
    Ok(r["character_id"].as_i64().unwrap())
}

// ── Roadmap assertions ────────────────────────────────────────────────────────

#[tokio::test]
async fn bless_contributes_a_rolled_die_to_persuasion() -> Result<()> {
    let h = connect().await?;
    let cid = make_character(&h.client, "Kira").await?;

    // Apply bless as an effect keyed to persuasion with a 1d4 dice_expr.
    call(
        &h.client,
        "apply_effect",
        serde_json::json!({
            "target_character_id": cid,
            "source": "spell:bless",
            "target_kind": "skill",
            "target_key": "persuasion",
            "modifier": 0,
            "dice_expr": "1d4"
        }),
    )
    .await?;

    // Resolve a persuasion check.
    let result = call(
        &h.client,
        "resolve_check",
        serde_json::json!({
            "character_id": cid,
            "kind": "skill_check",
            "target_key": "persuasion"
        }),
    )
    .await?;

    let effect_dice = result["effect_dice"]
        .as_array()
        .context("effect_dice array")?;
    assert_eq!(
        effect_dice.len(),
        1,
        "bless should roll exactly one extra die; got {effect_dice:?}"
    );
    let bless = &effect_dice[0];
    // The dice module renders single-die specs as "d4" (not "1d4"), so the spec label
    // will look like "spell:bless/d4". Assert that a d4 is the die shape in play.
    assert!(
        bless["spec"].as_str().unwrap_or("").contains("d4"),
        "die spec should mention a d4; got {bless:?}"
    );
    let value = bless["value"].as_i64().unwrap();
    assert!((1..=4).contains(&value), "bless die {value} out of [1, 4]");

    // Breakdown should mention the effect die.
    let breakdown = result["breakdown"].as_array().unwrap();
    assert!(
        breakdown
            .iter()
            .any(|b| b["kind"].as_str().unwrap_or("").starts_with("effect_die")),
        "breakdown should include an effect_die entry; got {breakdown:?}"
    );

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn blinded_forces_disadvantage_on_attack_rolls() -> Result<()> {
    let h = connect().await?;
    let cid = make_character(&h.client, "Kira").await?;

    call(
        &h.client,
        "condition.apply",
        serde_json::json!({
            "character_id": cid,
            "condition": "blinded",
            "severity": 1
        }),
    )
    .await?;

    let result = call(
        &h.client,
        "resolve_check",
        serde_json::json!({
            "character_id": cid,
            "kind": "attack_roll",
            "target_key": "attack",
            "ability": "str"
        }),
    )
    .await?;

    assert_eq!(
        result["posture"].as_str(),
        Some("disadvantage"),
        "blinded should impose disadvantage; result {result:#?}"
    );
    let d20s = result["d20s"].as_array().unwrap();
    assert_eq!(d20s.len(), 2, "disadvantage rolls 2d20");
    let used = result["d20_used"].as_i64().unwrap();
    let min = d20s.iter().map(|v| v.as_i64().unwrap()).min().unwrap();
    assert_eq!(used, min, "disadvantage keeps the lower d20");

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn ideology_alignment_modifier_threads_through_event_payload() -> Result<()> {
    let h = connect().await?;
    let cid = make_character(&h.client, "Kira").await?;

    let result = call(
        &h.client,
        "resolve_check",
        serde_json::json!({
            "character_id": cid,
            "kind": "skill_check",
            "target_key": "persuasion",
            "dc": 15,
            "modifiers": [
                {
                    "kind": "ideology_alignment",
                    "value": -6,
                    "reason": "cultist's demon-sacrifice goal vs. request to release captive"
                }
            ]
        }),
    )
    .await?;

    // Response body has the breakdown entry.
    let breakdown = result["breakdown"].as_array().unwrap();
    let found = breakdown
        .iter()
        .find(|b| b["kind"] == "ideology_alignment")
        .context("breakdown should have ideology_alignment")?;
    assert_eq!(found["value"].as_i64(), Some(-6));
    assert_eq!(
        found["reason"].as_str(),
        Some("cultist's demon-sacrifice goal vs. request to release captive")
    );

    // Closing the client flushes any outgoing writes before we read the file directly.
    h.client.cancel().await?;

    // Event payload has the modifier with its reason.
    let payload = latest_event_payload(&h.db_path, cid, "check.resolve");
    let mods = payload["modifiers"].as_array().unwrap();
    assert!(
        mods.iter().any(|m| m["kind"] == "ideology_alignment"
            && m["value"] == -6
            && m["reason"] == "cultist's demon-sacrifice goal vs. request to release captive"),
        "event payload modifiers should record the ideology_alignment entry verbatim; got {mods:?}"
    );

    Ok(())
}

// ── Regression test for issue #17 (effect source attribution in breakdown) ──

#[tokio::test]
async fn ability_targeting_effects_appear_in_resolve_check_breakdown() -> Result<()> {
    // Pre-fix, an ability-targeting effect (e.g. potion:giant-strength → +2 str_score)
    // got silently composed into the `effective_str` value behind the ability:str
    // breakdown line. The DM agent could see the score had jumped from 12 → 14 but
    // not WHY — no `effect:<source>` entry made it into the response. After the fix,
    // every ability-targeting effect with non-zero modifier emits its own
    // `effect:ability:<source>` line so the agent can attribute the change.
    let h = connect().await?;
    let cid = make_character(&h.client, "Brennan").await?;
    // make_character builds a STR 14 ranger; bump to a known starting point of 12 by
    // re-creating instead. Simpler: just use the existing 14 and apply +2 → effective 16.
    call(
        &h.client,
        "apply_effect",
        serde_json::json!({
            "target_character_id": cid,
            "source": "potion:giant-strength",
            "target_kind": "ability",
            "target_key": "str_score",
            "modifier": 2
        }),
    )
    .await?;

    let result = call(
        &h.client,
        "resolve_check",
        serde_json::json!({
            "character_id": cid,
            "kind": "ability_check",
            "target_key": "str"
        }),
    )
    .await?;

    let breakdown = result["breakdown"].as_array().context("breakdown")?;

    // The existing ability:str line should still be present and report the composed
    // effective score (14 base + 2 from potion = 16).
    let ability_line = breakdown
        .iter()
        .find(|b| b["kind"] == "ability:str")
        .context("breakdown should still include ability:str")?;
    assert!(
        ability_line["reason"]
            .as_str()
            .unwrap_or("")
            .contains("effective STR score 16"),
        "ability line should report the composed score 16; got {ability_line:?}"
    );

    // The new effect attribution line.
    let effect_line = breakdown
        .iter()
        .find(|b| b["kind"] == "effect:ability:potion:giant-strength")
        .context("breakdown should now include effect:ability:potion:giant-strength")?;
    assert_eq!(
        effect_line["value"].as_i64(),
        Some(2),
        "effect line value should be the raw modifier; got {effect_line:?}"
    );
    let reason = effect_line["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("potion:giant-strength") && reason.contains("STR"),
        "effect line reason should name the source and the ability; got {effect_line:?}"
    );

    h.client.cancel().await?;
    Ok(())
}

// ── Sanity: tools listed ──────────────────────────────────────────────────────

#[tokio::test]
async fn check_and_condition_tools_are_listed() -> Result<()> {
    let h = connect().await?;
    let tools = h.client.list_all_tools().await?;
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for required in ["resolve_check", "condition.apply", "condition.remove"] {
        assert!(
            names.contains(&required),
            "missing tool {required}; got {names:?}"
        );
    }
    h.client.cancel().await?;
    Ok(())
}
