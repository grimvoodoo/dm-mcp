//! E2E tests for Phase 4 (character core).
//!
//! The headline assertion is from the Roadmap:
//!
//!   Create character with STR 14 → apply_effect(+4 STR) → character.get shows effective
//!   18 → dispel_effect → shows 14. Event log has character.created, effect.applied,
//!   effect.expired.
//!
//! Additional tests cover update_plans / change_role / proficiencies / resources.

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

/// Invoke a tool with the given JSON args and parse the text payload as JSON.
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

/// Collect every event kind that referenced the given character, in insertion order. Reads
/// the SQLite file directly — the server is still running but SQLite's WAL mode lets a
/// read-only opener see committed rows without contending.
fn event_kinds_for(db_path: &std::path::Path, character_id: i64) -> Vec<String> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.kind FROM events e
             JOIN event_participants ep ON ep.event_id = e.id
             WHERE ep.character_id = ?1
             ORDER BY e.id",
        )
        .unwrap();
    stmt.query_map([character_id], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

// ── Headline E2E ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn str_effect_apply_get_dispel_roundtrip() -> Result<()> {
    let h = connect().await?;

    // 1. Create a character with STR 14.
    let create_result = call(
        &h.client,
        "character.create",
        serde_json::json!({
            "name": "Kira",
            "role": "player",
            "str_score": 14,
            "dex_score": 12,
            "con_score": 13,
            "int_score": 10,
            "wis_score": 11,
            "cha_score": 9
        }),
    )
    .await?;
    let character_id = create_result["character_id"]
        .as_i64()
        .context("character_id")?;

    // 2. character.get — effective STR should equal 14.
    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": character_id }),
    )
    .await?;
    assert_eq!(view["str_score"].as_i64(), Some(14));
    assert_eq!(view["effective_str"].as_i64(), Some(14));

    // 3. apply_effect +4 STR.
    let apply_result = call(
        &h.client,
        "apply_effect",
        serde_json::json!({
            "target_character_id": character_id,
            "source": "potion:bulls-strength",
            "target_kind": "ability",
            "target_key": "str_score",
            "modifier": 4
        }),
    )
    .await?;
    let effect_id = apply_result["effect_id"].as_i64().context("effect_id")?;

    // 4. character.get — effective STR now 18 (base 14 + 4).
    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": character_id }),
    )
    .await?;
    assert_eq!(view["str_score"].as_i64(), Some(14), "base unchanged");
    assert_eq!(
        view["effective_str"].as_i64(),
        Some(18),
        "effective = base + effect; view = {view:#?}"
    );

    // 5. dispel_effect.
    call(
        &h.client,
        "dispel_effect",
        serde_json::json!({ "effect_id": effect_id, "reason": "potion wore off" }),
    )
    .await?;

    // 6. character.get — effective STR back to 14.
    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": character_id }),
    )
    .await?;
    assert_eq!(
        view["effective_str"].as_i64(),
        Some(14),
        "after dispel, effective == base"
    );

    // Close the MCP session cleanly — otherwise the child's SQLite connection is still
    // open and querying the file read-only can race with an uncommitted writer in some
    // edge cases. cancel() shuts down the child.
    h.client.cancel().await?;

    // 7. Event log has the three required kinds referencing this character.
    let kinds = event_kinds_for(&h.db_path, character_id);
    for required in ["character.created", "effect.applied", "effect.expired"] {
        assert!(
            kinds.iter().any(|k| k == required),
            "event log should include {required}; got {kinds:?}"
        );
    }

    Ok(())
}

// ── Side tools ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn update_plans_changes_row_and_logs_event() -> Result<()> {
    let h = connect().await?;
    let create_result = call(
        &h.client,
        "character.create",
        serde_json::json!({
            "name": "Bob",
            "role": "friendly",
            "str_score": 8, "dex_score": 10, "con_score": 10,
            "int_score": 12, "wis_score": 14, "cha_score": 13
        }),
    )
    .await?;
    let character_id = create_result["character_id"].as_i64().unwrap();

    call(
        &h.client,
        "character.update_plans",
        serde_json::json!({
            "character_id": character_id,
            "new_plans": "Find the missing smith",
            "reason": "village hooks"
        }),
    )
    .await?;

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": character_id }),
    )
    .await?;
    assert_eq!(view["plans"].as_str(), Some("Find the missing smith"));

    h.client.cancel().await?;
    let kinds = event_kinds_for(&h.db_path, character_id);
    assert!(kinds.contains(&"npc.plan_changed".into()));
    Ok(())
}

#[tokio::test]
async fn change_role_flips_role_and_logs_event() -> Result<()> {
    let h = connect().await?;
    let create_result = call(
        &h.client,
        "character.create",
        serde_json::json!({
            "name": "Grog",
            "role": "enemy",
            "str_score": 16, "dex_score": 11, "con_score": 15,
            "int_score": 7, "wis_score": 9, "cha_score": 8
        }),
    )
    .await?;
    let character_id = create_result["character_id"].as_i64().unwrap();

    call(
        &h.client,
        "character.change_role",
        serde_json::json!({
            "character_id": character_id,
            "new_role": "companion",
            "reason": "rescued from captivity; now pledged to the party"
        }),
    )
    .await?;

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": character_id }),
    )
    .await?;
    assert_eq!(view["role"].as_str(), Some("companion"));

    h.client.cancel().await?;
    assert!(event_kinds_for(&h.db_path, character_id).contains(&"npc.role_changed".into()));
    Ok(())
}

#[tokio::test]
async fn effects_stack_additively_on_same_target() -> Result<()> {
    // Two effects both targeting str_score should compose at read time:
    // effective_str = base + sum(modifiers).
    let h = connect().await?;
    let cr = call(
        &h.client,
        "character.create",
        serde_json::json!({
            "name": "Stacker",
            "role": "player",
            "str_score": 12, "dex_score": 10, "con_score": 10,
            "int_score": 10, "wis_score": 10, "cha_score": 10
        }),
    )
    .await?;
    let cid = cr["character_id"].as_i64().unwrap();

    call(
        &h.client,
        "apply_effect",
        serde_json::json!({
            "target_character_id": cid,
            "source": "spell:bull-strength",
            "target_kind": "ability",
            "target_key": "str_score",
            "modifier": 2
        }),
    )
    .await?;
    call(
        &h.client,
        "apply_effect",
        serde_json::json!({
            "target_character_id": cid,
            "source": "rage",
            "target_kind": "ability",
            "target_key": "str_score",
            "modifier": 1
        }),
    )
    .await?;

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": cid }),
    )
    .await?;
    assert_eq!(view["str_score"].as_i64(), Some(12), "base unchanged");
    assert_eq!(
        view["effective_str"].as_i64(),
        Some(15),
        "effective = 12 + 2 + 1; view = {view:#?}"
    );
    let effects = view["active_effects"].as_array().unwrap();
    assert_eq!(
        effects.len(),
        2,
        "both effects should be active; got {effects:?}"
    );

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn proficiency_and_resource_crud() -> Result<()> {
    let h = connect().await?;
    let create_result = call(
        &h.client,
        "character.create",
        serde_json::json!({
            "name": "Kira",
            "role": "player",
            "str_score": 10, "dex_score": 14, "con_score": 12,
            "int_score": 10, "wis_score": 10, "cha_score": 10
        }),
    )
    .await?;
    let character_id = create_result["character_id"].as_i64().unwrap();

    // proficiency.set with expertise
    call(
        &h.client,
        "proficiency.set",
        serde_json::json!({
            "character_id": character_id,
            "name": "stealth",
            "proficient": true,
            "expertise": true
        }),
    )
    .await?;

    // resource.set + resource.adjust + resource.remove
    call(
        &h.client,
        "resource.set",
        serde_json::json!({
            "character_id": character_id,
            "name": "slot:1",
            "current": 4, "max": 4,
            "recharge": "long_rest"
        }),
    )
    .await?;
    let adjusted = call(
        &h.client,
        "resource.adjust",
        serde_json::json!({
            "character_id": character_id,
            "name": "slot:1",
            "delta": -1,
            "reason": "cast Magic Missile"
        }),
    )
    .await?;
    assert_eq!(adjusted["current"].as_i64(), Some(3));

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": character_id }),
    )
    .await?;
    let profs = view["proficiencies"].as_array().unwrap();
    assert!(
        profs
            .iter()
            .any(|p| p["name"] == "stealth" && p["expertise"].as_bool() == Some(true)),
        "proficiencies should include expert stealth; got {profs:?}"
    );
    let resources = view["resources"].as_array().unwrap();
    assert!(
        resources
            .iter()
            .any(|r| r["name"] == "slot:1" && r["current"].as_i64() == Some(3)),
        "resources should show slot:1 at 3/4; got {resources:?}"
    );

    // Remove both.
    call(
        &h.client,
        "proficiency.remove",
        serde_json::json!({ "character_id": character_id, "name": "stealth" }),
    )
    .await?;
    call(
        &h.client,
        "resource.remove",
        serde_json::json!({ "character_id": character_id, "name": "slot:1" }),
    )
    .await?;

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": character_id }),
    )
    .await?;
    assert!(
        view["proficiencies"].as_array().unwrap().is_empty(),
        "proficiencies should be empty after remove"
    );
    assert!(
        view["resources"].as_array().unwrap().is_empty(),
        "resources should be empty after remove"
    );

    h.client.cancel().await?;
    Ok(())
}
