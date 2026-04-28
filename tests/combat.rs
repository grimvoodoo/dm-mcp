//! E2E tests for Phase 9 (encounters + combat + death + rests).
//!
//! Roadmap assertions:
//!
//!   Start encounter → combat → `next_turn` × N with 2-round effect → expired after round
//!   3. Damage to 0 HP → `mortally_wounded` applied, status='unconscious'. Three failed
//!   death saves → status='dead' → `roll_death_event` returns a rolled event. Starting a
//!   second combat while first still flagged → first auto-ended, `combat.auto_ended`
//!   emitted.

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

async fn make_char(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    name: &str,
    role: &str,
    hp_max: i64,
) -> Result<i64> {
    let r = call(
        client,
        "character.create",
        serde_json::json!({
            "name": name,
            "role": role,
            "str_score": 12,
            "dex_score": 12,
            "con_score": 12,
            "int_score": 10,
            "wis_score": 10,
            "cha_score": 10,
            "hp_max": hp_max,
            "armor_class": 13,
            "initiative_bonus": 1
        }),
    )
    .await?;
    Ok(r["character_id"].as_i64().unwrap())
}

async fn make_encounter(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    players: &[i64],
    hostiles: &[i64],
    xp_budget: i64,
) -> Result<i64> {
    let participants: Vec<serde_json::Value> = players
        .iter()
        .map(|id| serde_json::json!({ "character_id": id, "side": "player_side" }))
        .chain(
            hostiles
                .iter()
                .map(|id| serde_json::json!({ "character_id": id, "side": "hostile" })),
        )
        .collect();
    let r = call(
        client,
        "encounter.create",
        serde_json::json!({
            "name": "test encounter",
            "goal": "survive",
            "estimated_duration_hours": 1,
            "xp_budget": xp_budget,
            "participants": participants
        }),
    )
    .await?;
    Ok(r["encounter_id"].as_i64().unwrap())
}

/// Drop the character to 0 HP and roll death saves until they hit `status: "dead"`.
/// Used by guard tests that need a dead victim to verify the dead-character refusal paths.
async fn kill_character(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    character_id: i64,
) -> Result<()> {
    call(
        client,
        "combat.apply_damage",
        serde_json::json!({ "character_id": character_id, "amount": 999 }),
    )
    .await?;
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 300, "never reached dead status; guard tripped");
        let save = call(
            client,
            "roll_death_save",
            serde_json::json!({ "character_id": character_id }),
        )
        .await?;
        match save["status"].as_str() {
            Some("dead") => return Ok(()),
            Some("alive") => {
                // Stabilised — knock them down again.
                call(
                    client,
                    "combat.apply_damage",
                    serde_json::json!({ "character_id": character_id, "amount": 999 }),
                )
                .await?;
            }
            _ => { /* still unconscious — keep rolling */ }
        }
    }
}

// ── E2E 1: round-based effect expiry ────────────────────────────────────────

#[tokio::test]
async fn two_round_effect_expires_after_round_three() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 20).await?;
    let enemy = make_char(&h.client, "Goblin", "enemy", 10).await?;
    let eid = make_encounter(&h.client, &[player], &[enemy], 100).await?;

    call(
        &h.client,
        "combat.start",
        serde_json::json!({ "encounter_id": eid }),
    )
    .await?;

    let apply = call(
        &h.client,
        "apply_effect",
        serde_json::json!({
            "target_character_id": player,
            "source": "spell:bless",
            "target_kind": "attack",
            "target_key": "attack",
            "modifier": 0,
            "dice_expr": "1d4",
            "expires_after_rounds": 2,
            "expires_on_dispel": true
        }),
    )
    .await?;
    let effect_id = apply["effect_id"].as_i64().unwrap();

    // Walk through the turn sequence: 2 participants, so every 2 next_turns wraps a round.
    // Round 1 → round 2: effect ticks 2→1 (still active)
    // Round 2 → round 3: effect ticks 1→0 (expires)
    let r = call(
        &h.client,
        "combat.next_turn",
        serde_json::json!({ "encounter_id": eid }),
    )
    .await?;
    assert_eq!(r["wrapped_to_new_round"].as_bool(), Some(false));

    let r = call(
        &h.client,
        "combat.next_turn",
        serde_json::json!({ "encounter_id": eid }),
    )
    .await?;
    assert_eq!(r["wrapped_to_new_round"].as_bool(), Some(true));
    assert_eq!(r["current_round"].as_i64(), Some(2));
    assert!(
        r["expired_effect_ids"].as_array().unwrap().is_empty(),
        "effect should still be active after round 2 tick"
    );

    let r = call(
        &h.client,
        "combat.next_turn",
        serde_json::json!({ "encounter_id": eid }),
    )
    .await?;
    assert_eq!(r["wrapped_to_new_round"].as_bool(), Some(false));

    let r = call(
        &h.client,
        "combat.next_turn",
        serde_json::json!({ "encounter_id": eid }),
    )
    .await?;
    assert_eq!(r["wrapped_to_new_round"].as_bool(), Some(true));
    assert_eq!(r["current_round"].as_i64(), Some(3));
    let expired = r["expired_effect_ids"].as_array().unwrap();
    assert!(
        expired.iter().any(|v| v.as_i64() == Some(effect_id)),
        "effect should expire at round 3 boundary; got {expired:?}"
    );

    h.client.cancel().await?;
    Ok(())
}

// ── E2E 2: damage to 0 HP → mortally_wounded + unconscious ─────────────────

#[tokio::test]
async fn damage_to_zero_triggers_death_flow() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 10).await?;

    let r = call(
        &h.client,
        "combat.apply_damage",
        serde_json::json!({
            "character_id": player,
            "amount": 15,
            "damage_type": "slashing",
            "source": "orc greataxe"
        }),
    )
    .await?;
    assert_eq!(r["hp_current"].as_i64(), Some(0));
    assert_eq!(r["status"].as_str(), Some("unconscious"));
    assert_eq!(r["newly_unconscious"].as_bool(), Some(true));
    assert!(r["mortally_wounded_condition_id"].as_i64().is_some());

    // Cross-check via character.get
    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    assert_eq!(view["status"].as_str(), Some("unconscious"));
    let conds = view["active_conditions"].as_array().unwrap();
    assert!(
        conds.iter().any(|c| c["condition"] == "mortally_wounded"),
        "mortally_wounded should be active; got {conds:?}"
    );

    h.client.cancel().await?;
    Ok(())
}

// ── E2E 3: three failed saves → dead → roll_death_event returns something ──

#[tokio::test]
async fn three_failed_death_saves_end_with_rolled_event() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 10).await?;
    call(
        &h.client,
        "combat.apply_damage",
        serde_json::json!({ "character_id": player, "amount": 30 }),
    )
    .await?;

    // Keep rolling until three failures land. Each successful/auto_stabilise roll resets
    // the counters so we can try again; this matches the game flow (the character gets
    // knocked down again). In practice this finishes in a handful of rolls.
    let mut guard = 0;
    let death_save_result = loop {
        guard += 1;
        assert!(guard < 300, "could not achieve 3 failures; guard tripped");
        let save = call(
            &h.client,
            "roll_death_save",
            serde_json::json!({ "character_id": player }),
        )
        .await?;
        if save["status"].as_str() == Some("dead") {
            break save;
        }
        // Not dead — reset HP to 0 and re-apply mortally_wounded to try again.
        if save["status"].as_str() == Some("alive") {
            // Drop them again.
            call(
                &h.client,
                "combat.apply_damage",
                serde_json::json!({ "character_id": player, "amount": 30 }),
            )
            .await?;
        }
    };
    assert_eq!(death_save_result["failures"].as_i64(), Some(3));

    let de = call(
        &h.client,
        "roll_death_event",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    let kind = de["rolled"]["kind"].as_str().context("rolled.kind")?;
    assert!(!kind.is_empty(), "rolled death event should have a kind");

    h.client.cancel().await?;
    Ok(())
}

// ── E2E 4: second combat auto-ends the first ───────────────────────────────

#[tokio::test]
async fn starting_second_combat_auto_ends_first() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 20).await?;
    let enemy1 = make_char(&h.client, "Goblin 1", "enemy", 10).await?;
    let enemy2 = make_char(&h.client, "Goblin 2", "enemy", 10).await?;
    let eid1 = make_encounter(&h.client, &[player], &[enemy1], 50).await?;
    let eid2 = make_encounter(&h.client, &[player], &[enemy2], 50).await?;

    call(
        &h.client,
        "combat.start",
        serde_json::json!({ "encounter_id": eid1 }),
    )
    .await?;

    let r = call(
        &h.client,
        "combat.start",
        serde_json::json!({ "encounter_id": eid2 }),
    )
    .await?;
    assert_eq!(
        r["auto_ended_encounter_id"].as_i64(),
        Some(eid1),
        "second combat.start should auto-end the first; got {r:#?}"
    );

    h.client.cancel().await?;

    // Confirm a combat.auto_ended event is in the log on the first encounter.
    let conn = Connection::open_with_flags(
        &h.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events
             WHERE kind = 'combat.auto_ended' AND encounter_id = ?1",
            [eid1],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "expected one combat.auto_ended event on first encounter"
    );

    // And first encounter is no longer in combat.
    let in_combat: i64 = conn
        .query_row(
            "SELECT in_combat FROM encounters WHERE id = ?1",
            [eid1],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(in_combat, 0);
    Ok(())
}

// ── Ancillary: encounter.complete awards XP to player_side ──────────────────

#[tokio::test]
async fn encounter_complete_awards_xp_and_bypass_earns_same() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 20).await?;
    let enemy = make_char(&h.client, "Goblin", "enemy", 10).await?;
    let eid = make_encounter(&h.client, &[player], &[enemy], 200).await?;

    let r = call(
        &h.client,
        "encounter.complete",
        serde_json::json!({
            "encounter_id": eid,
            "path": "stealth_bypass"
        }),
    )
    .await?;
    assert_eq!(r["xp_awarded_total"].as_i64(), Some(200));
    assert_eq!(r["status"].as_str(), Some("goal_completed"));

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    assert_eq!(
        view["xp_total"].as_i64(),
        Some(200),
        "bypass should earn full XP — this is the 'no kill XP' decision"
    );

    h.client.cancel().await?;
    Ok(())
}

// ── Ancillary: short/long rest tools ────────────────────────────────────────

#[tokio::test]
async fn rests_refill_resources_and_heal() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 20).await?;
    call(
        &h.client,
        "resource.set",
        serde_json::json!({
            "character_id": player,
            "name": "slot:1",
            "current": 0, "max": 4,
            "recharge": "long_rest"
        }),
    )
    .await?;
    call(
        &h.client,
        "resource.set",
        serde_json::json!({
            "character_id": player,
            "name": "hit_die",
            "current": 0, "max": 2,
            "recharge": "short_rest"
        }),
    )
    .await?;
    // Take 10 damage.
    call(
        &h.client,
        "combat.apply_damage",
        serde_json::json!({ "character_id": player, "amount": 10 }),
    )
    .await?;

    // Short rest: hit_die refills; slot:1 does not; HP unchanged.
    let r = call(
        &h.client,
        "rest.short",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    let refilled: Vec<&str> = r["refilled_resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(refilled.contains(&"hit_die"));
    assert!(!refilled.contains(&"slot:1"));
    assert!(r["hp_restored"].is_null());

    // Long rest: slot:1 refills; HP restored to max.
    let r = call(
        &h.client,
        "rest.long",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    let refilled: Vec<&str> = r["refilled_resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(refilled.contains(&"slot:1"));
    assert_eq!(r["hp_restored"].as_i64(), Some(10));

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    assert_eq!(view["hp_current"].as_i64(), Some(20));
    h.client.cancel().await?;
    Ok(())
}

// ── Healing path: heal-above-zero clears mortally_wounded ──────────────────

#[tokio::test]
async fn healing_above_zero_clears_mortally_wounded() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 12).await?;

    // Drop to 0 — auto-applies mortally_wounded + unconscious.
    let r = call(
        &h.client,
        "combat.apply_damage",
        serde_json::json!({ "character_id": player, "amount": 12 }),
    )
    .await?;
    assert_eq!(r["status"].as_str(), Some("unconscious"));
    let mw_id = r["mortally_wounded_condition_id"]
        .as_i64()
        .context("mortally_wounded_condition_id")?;

    // Heal above 0 — must clear mortally_wounded.
    let h_r = call(
        &h.client,
        "combat.apply_healing",
        serde_json::json!({ "character_id": player, "amount": 5 }),
    )
    .await?;
    assert_eq!(h_r["hp_current"].as_i64(), Some(5));
    assert_eq!(h_r["status"].as_str(), Some("alive"));
    assert_eq!(h_r["cleared_mortally_wounded"].as_bool(), Some(true));

    // Cross-check via character.get: condition row no longer active.
    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    let conds = view["active_conditions"].as_array().unwrap();
    assert!(
        !conds.iter().any(|c| c["id"].as_i64() == Some(mw_id)),
        "mortally_wounded should be cleared after heal-above-zero; got {conds:?}"
    );

    h.client.cancel().await?;
    Ok(())
}

// ── Dead-character guards: heal / rest.long / roll_death_save all refuse ──

#[tokio::test]
async fn healing_dead_character_refused() -> Result<()> {
    use rmcp::model::CallToolRequestParams;

    let h = connect().await?;
    let player = make_char(&h.client, "Doomed", "player", 5).await?;
    kill_character(&h.client, player).await?;

    let args = serde_json::json!({ "character_id": player, "amount": 10 });
    let params = CallToolRequestParams::new("combat.apply_healing")
        .with_arguments(args.as_object().unwrap().clone());
    let outcome = h.client.call_tool(params).await;
    match outcome {
        Err(_) => { /* JSON-RPC error — fine */ }
        Ok(result) => assert_eq!(
            result.is_error,
            Some(true),
            "healing dead character must signal error; got {result:?}"
        ),
    }

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn damaging_dead_character_refused() -> Result<()> {
    // Regression for #31: apply_damage on a corpse used to silently clamp HP to 0
    // again and emit a spurious damage event. Now refuses, mirroring apply_healing.
    use rmcp::model::CallToolRequestParams;

    let h = connect().await?;
    let player = make_char(&h.client, "Doomed", "player", 5).await?;
    kill_character(&h.client, player).await?;

    let args = serde_json::json!({ "character_id": player, "amount": 10 });
    let params = CallToolRequestParams::new("combat.apply_damage")
        .with_arguments(args.as_object().unwrap().clone());
    let outcome = h.client.call_tool(params).await;
    match outcome {
        Err(_) => { /* JSON-RPC error — fine */ }
        Ok(result) => assert_eq!(
            result.is_error,
            Some(true),
            "damaging dead character must signal error; got {result:?}"
        ),
    }

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn rest_long_refused_when_dead() -> Result<()> {
    use rmcp::model::CallToolRequestParams;

    let h = connect().await?;
    let player = make_char(&h.client, "Doomed", "player", 5).await?;
    kill_character(&h.client, player).await?;

    let args = serde_json::json!({ "character_id": player });
    let params =
        CallToolRequestParams::new("rest.long").with_arguments(args.as_object().unwrap().clone());
    let outcome = h.client.call_tool(params).await;
    match outcome {
        Err(_) => {}
        Ok(result) => assert_eq!(
            result.is_error,
            Some(true),
            "rest.long on dead character must signal error; got {result:?}"
        ),
    }

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn death_save_refused_when_dead() -> Result<()> {
    use rmcp::model::CallToolRequestParams;

    let h = connect().await?;
    let player = make_char(&h.client, "Doomed", "player", 5).await?;
    kill_character(&h.client, player).await?;

    let args = serde_json::json!({ "character_id": player });
    let params = CallToolRequestParams::new("roll_death_save")
        .with_arguments(args.as_object().unwrap().clone());
    let outcome = h.client.call_tool(params).await;
    match outcome {
        Err(_) => {}
        Ok(result) => assert_eq!(
            result.is_error,
            Some(true),
            "roll_death_save on dead character must signal error; got {result:?}"
        ),
    }

    h.client.cancel().await?;
    Ok(())
}

// ── Death save edge cases: nat-1 = 2 failures, nat-20 = auto-stabilise ─────

#[tokio::test]
async fn death_save_nat1_critical_fails_two_steps() -> Result<()> {
    // Roll death saves on a fresh victim, re-dropping after stabilisation, until we land
    // a nat-1 (`outcome: "critical_fail"`). Each nat-1 must:
    //   - bump failures by exactly 2
    //   - have d20 = 1
    // Generates a new victim if the current one dies via plain failures.
    let h = connect().await?;
    let mut victim = make_char(&h.client, "Victim 0", "player", 10).await?;
    call(
        &h.client,
        "combat.apply_damage",
        serde_json::json!({ "character_id": victim, "amount": 30 }),
    )
    .await?;

    let mut guard = 0;
    let mut prev_failures = 0;
    let mut victim_idx = 0;
    let result = loop {
        guard += 1;
        assert!(guard < 500, "never observed a nat-1 in 500 rolls");
        let save = call(
            &h.client,
            "roll_death_save",
            serde_json::json!({ "character_id": victim }),
        )
        .await?;
        if save["outcome"].as_str() == Some("critical_fail") {
            break save;
        }
        match save["status"].as_str() {
            Some("alive") => {
                // Stabilised via 3 successes — re-drop.
                call(
                    &h.client,
                    "combat.apply_damage",
                    serde_json::json!({ "character_id": victim, "amount": 30 }),
                )
                .await?;
                prev_failures = 0;
            }
            Some("dead") => {
                // Died via plain fails before we got our nat-1 — fresh victim.
                victim_idx += 1;
                victim =
                    make_char(&h.client, &format!("Victim {victim_idx}"), "player", 10).await?;
                call(
                    &h.client,
                    "combat.apply_damage",
                    serde_json::json!({ "character_id": victim, "amount": 30 }),
                )
                .await?;
                prev_failures = 0;
            }
            _ => {
                prev_failures = save["failures"].as_i64().unwrap();
            }
        }
    };
    assert_eq!(
        result["d20"].as_i64(),
        Some(1),
        "critical_fail requires d20=1"
    );
    let now_failures = result["failures"].as_i64().unwrap();
    assert_eq!(
        now_failures,
        (prev_failures + 2).min(3),
        "nat-1 should bump failures by 2 (clamped to 3); was {prev_failures}, now {now_failures}"
    );

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn death_save_nat20_auto_stabilises_at_one_hp() -> Result<()> {
    // Roll death saves until a nat-20 fires. On nat-20:
    //   - outcome = "auto_stabilise"
    //   - status = "alive", hp_current = 1
    //   - successes/failures both 0
    //   - mortally_wounded condition cleared from active_conditions
    let h = connect().await?;
    let mut victim = make_char(&h.client, "Victim 0", "player", 10).await?;
    call(
        &h.client,
        "combat.apply_damage",
        serde_json::json!({ "character_id": victim, "amount": 30 }),
    )
    .await?;

    let mut guard = 0;
    let mut victim_idx = 0;
    let result = loop {
        guard += 1;
        assert!(guard < 500, "never observed a nat-20 in 500 rolls");
        let save = call(
            &h.client,
            "roll_death_save",
            serde_json::json!({ "character_id": victim }),
        )
        .await?;
        if save["outcome"].as_str() == Some("auto_stabilise") {
            break save;
        }
        match save["status"].as_str() {
            Some("alive") => {
                // Stabilised via 3 successes (not via nat-20 since outcome differs) — re-drop.
                call(
                    &h.client,
                    "combat.apply_damage",
                    serde_json::json!({ "character_id": victim, "amount": 30 }),
                )
                .await?;
            }
            Some("dead") => {
                victim_idx += 1;
                victim =
                    make_char(&h.client, &format!("Victim {victim_idx}"), "player", 10).await?;
                call(
                    &h.client,
                    "combat.apply_damage",
                    serde_json::json!({ "character_id": victim, "amount": 30 }),
                )
                .await?;
            }
            _ => { /* unconscious, keep rolling */ }
        }
    };
    assert_eq!(
        result["d20"].as_i64(),
        Some(20),
        "auto_stabilise requires d20=20"
    );
    assert_eq!(result["status"].as_str(), Some("alive"));
    assert_eq!(result["successes"].as_i64(), Some(0));
    assert_eq!(result["failures"].as_i64(), Some(0));

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": victim }),
    )
    .await?;
    assert_eq!(view["hp_current"].as_i64(), Some(1), "nat-20 sets hp to 1");
    let conds = view["active_conditions"].as_array().unwrap();
    assert!(
        !conds.iter().any(|c| c["condition"] == "mortally_wounded"),
        "auto_stabilise should clear mortally_wounded; got {conds:?}"
    );

    h.client.cancel().await?;
    Ok(())
}

// ── Encounter outcomes: xp_modifier, abandon, fail ─────────────────────────

#[tokio::test]
async fn encounter_complete_xp_modifier_scales_award() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 20).await?;
    let enemy = make_char(&h.client, "Goblin", "enemy", 10).await?;
    let eid = make_encounter(&h.client, &[player], &[enemy], 50).await?;

    let r = call(
        &h.client,
        "encounter.complete",
        serde_json::json!({
            "encounter_id": eid,
            "path": "combat_victory",
            "xp_modifier": 2.0
        }),
    )
    .await?;
    assert_eq!(r["status"].as_str(), Some("goal_completed"));
    assert_eq!(
        r["xp_awarded_total"].as_i64(),
        Some(100),
        "50 budget * 2.0 modifier = 100"
    );

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    assert_eq!(view["xp_total"].as_i64(), Some(100));

    h.client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn encounter_abandon_awards_no_xp_and_flips_status() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 20).await?;
    let enemy = make_char(&h.client, "Wolf", "enemy", 10).await?;
    let eid = make_encounter(&h.client, &[player], &[enemy], 100).await?;

    let r = call(
        &h.client,
        "encounter.abandon",
        serde_json::json!({
            "encounter_id": eid,
            "reason": "player chose to circle wide"
        }),
    )
    .await?;
    assert_eq!(r["status"].as_str(), Some("abandoned"));

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    assert_eq!(view["xp_total"].as_i64(), Some(0), "abandon awards no XP");

    h.client.cancel().await?;

    // Confirm the encounter.abandoned event landed in the log.
    let conn = Connection::open_with_flags(
        &h.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE kind = 'encounter.abandoned' AND encounter_id = ?1",
        [eid],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
async fn encounter_fail_awards_no_xp_and_flips_status() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 20).await?;
    let enemy = make_char(&h.client, "Tower", "enemy", 10).await?;
    let eid = make_encounter(&h.client, &[player], &[enemy], 100).await?;

    let r = call(
        &h.client,
        "encounter.fail",
        serde_json::json!({
            "encounter_id": eid,
            "reason": "sun set before reaching the watchtower"
        }),
    )
    .await?;
    assert_eq!(r["status"].as_str(), Some("failed"));

    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    assert_eq!(view["xp_total"].as_i64(), Some(0), "fail awards no XP");

    h.client.cancel().await?;

    let conn = Connection::open_with_flags(
        &h.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE kind = 'encounter.failed' AND encounter_id = ?1",
        [eid],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    Ok(())
}

// ── Conditions tick down on round wrap (parallel to the effect-expiry test) ─

#[tokio::test]
async fn condition_with_expires_after_rounds_decrements_on_round_wrap() -> Result<()> {
    let h = connect().await?;
    let player = make_char(&h.client, "Kira", "player", 20).await?;
    let enemy = make_char(&h.client, "Goblin", "enemy", 10).await?;
    let eid = make_encounter(&h.client, &[player], &[enemy], 100).await?;
    call(
        &h.client,
        "combat.start",
        serde_json::json!({ "encounter_id": eid }),
    )
    .await?;

    // Apply a 1-round condition to the player.
    let cond = call(
        &h.client,
        "condition.apply",
        serde_json::json!({
            "character_id": player,
            "condition": "frightened",
            "severity": 1,
            "expires_after_rounds": 1
        }),
    )
    .await?;
    let cond_id = cond["condition_id"].as_i64().unwrap();

    // 2 participants → every 2 next_turns = 1 round wrap. After the first wrap (round 1→2)
    // the condition should tick from 1 → 0 and deactivate.
    let _ = call(
        &h.client,
        "combat.next_turn",
        serde_json::json!({ "encounter_id": eid }),
    )
    .await?;
    let r = call(
        &h.client,
        "combat.next_turn",
        serde_json::json!({ "encounter_id": eid }),
    )
    .await?;
    assert_eq!(r["wrapped_to_new_round"].as_bool(), Some(true));
    let expired = r["expired_condition_ids"].as_array().unwrap();
    assert!(
        expired.iter().any(|v| v.as_i64() == Some(cond_id)),
        "condition should expire on first round-wrap; got {expired:?}"
    );

    // Player no longer has frightened.
    let view = call(
        &h.client,
        "character.get",
        serde_json::json!({ "character_id": player }),
    )
    .await?;
    let conds = view["active_conditions"].as_array().unwrap();
    assert!(
        !conds.iter().any(|c| c["id"].as_i64() == Some(cond_id)),
        "expired condition should be gone from active_conditions; got {conds:?}"
    );

    h.client.cancel().await?;
    Ok(())
}
