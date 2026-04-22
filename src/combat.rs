//! Combat mode (a flag on an encounter) plus damage + death flow.
//!
//! Phase 9 surface:
//!
//! - [`start`] — enter combat mode on an encounter. Auto-ends any other currently
//!   in-combat encounter first (emitting `combat.auto_ended` on it) so a stale
//!   combat from a previous scene can't clutter the next fight.
//! - [`next_turn`] — advance the initiative pointer. At the round boundary, tick
//!   round-based effect / condition expiry.
//! - [`end`] — leave combat mode, zero out the combat-only participant fields.
//! - [`apply_damage`] / [`apply_healing`] — HP deltas. Damage to 0 triggers the
//!   death flow (mortally_wounded + unconscious); healing from mortally_wounded
//!   clears the condition and resets death-save counters.
//! - [`roll_death_save`] — d20 roll against DC 10, with nat-1 double-fail and
//!   nat-20 auto-stabilise per `docs/characters.md §Death`.
//! - [`roll_death_event`] — weighted pick from `content/rules/death_events.yaml`
//!   once a character has three death-save failures.
//!
//! See also `docs/encounters.md §Combat flow` for the design rationale.

use anyhow::{bail, Context, Result};
use rand::RngExt;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::content::{Content, DeathEvent};
use crate::encounters::{read_encounter_or_404, EncounterRow};
use crate::events::{self, EventSpec, Participant};

// ── Tool params / results ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StartParams {
    pub encounter_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartResult {
    pub encounter_id: i64,
    pub current_round: i32,
    pub initiative_order: Vec<InitiativeEntry>,
    pub auto_ended_encounter_id: Option<i64>,
    pub event_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitiativeEntry {
    pub character_id: i64,
    pub side: String,
    pub initiative: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NextTurnParams {
    pub encounter_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NextTurnResult {
    pub encounter_id: i64,
    pub current_round: i32,
    pub current_character_id: i64,
    pub wrapped_to_new_round: bool,
    pub expired_effect_ids: Vec<i64>,
    pub expired_condition_ids: Vec<i64>,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EndParams {
    pub encounter_id: i64,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EndResult {
    pub encounter_id: i64,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ApplyDamageParams {
    pub character_id: i64,
    pub amount: i32,
    #[serde(default)]
    pub damage_type: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub encounter_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyDamageResult {
    pub character_id: i64,
    pub hp_current: i32,
    pub status: String,
    pub newly_unconscious: bool,
    pub mortally_wounded_condition_id: Option<i64>,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ApplyHealingParams {
    pub character_id: i64,
    pub amount: i32,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyHealingResult {
    pub character_id: i64,
    pub hp_current: i32,
    pub status: String,
    pub cleared_mortally_wounded: bool,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DeathSaveParams {
    pub character_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeathSaveResult {
    pub character_id: i64,
    pub d20: i32,
    pub outcome: String,
    pub successes: i32,
    pub failures: i32,
    pub status: String,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DeathEventParams {
    pub character_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeathEventResult {
    pub character_id: i64,
    pub rolled: DeathEvent,
    pub event_id: i64,
}

// ── combat.start ─────────────────────────────────────────────────────────────

pub fn start(conn: &mut Connection, p: StartParams) -> Result<StartResult> {
    let row = read_encounter_or_404(conn, p.encounter_id)?;
    if row.status != "active" {
        bail!(
            "cannot start combat on encounter {} (status {:?})",
            p.encounter_id,
            row.status
        );
    }
    if row.in_combat {
        bail!("encounter {} is already in combat", p.encounter_id);
    }

    let now = crate::world::current_campaign_hour(conn)?;

    // Find any other in-combat encounter to auto-end.
    let stale: Option<i64> = conn
        .query_row(
            "SELECT id FROM encounters WHERE in_combat = 1 AND id != ?1",
            [p.encounter_id],
            |row| row.get(0),
        )
        .optional_ok()?;

    // Participants + their initiative bonuses. Roll up-front so the tx body is mechanical.
    let mut stmt = conn.prepare(
        "SELECT ep.character_id, ep.side, c.initiative_bonus
         FROM encounter_participants ep
         JOIN characters c ON c.id = ep.character_id
         WHERE ep.encounter_id = ?1",
    )?;
    let parts: Vec<(i64, String, i32)> = stmt
        .query_map([p.encounter_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut rng = rand::rng();
    let mut with_init: Vec<(i64, String, i32)> = parts
        .into_iter()
        .map(|(cid, side, bonus)| (cid, side, rng.random_range(1..=20i32) + bonus))
        .collect();
    // Sort initiative desc; stable so ties follow insertion order.
    with_init.sort_by_key(|entry| std::cmp::Reverse(entry.2));

    let tx = conn.transaction().context("begin combat.start tx")?;

    let mut auto_ended_id: Option<i64> = None;
    if let Some(stale_id) = stale {
        end_combat_in_tx(&tx, stale_id, now, "superseded_by_new_combat", true)?;
        auto_ended_id = Some(stale_id);
    }

    tx.execute(
        "UPDATE encounters SET in_combat = 1, current_round = 1, turn_index = 0
         WHERE id = ?1",
        [p.encounter_id],
    )
    .context("flag encounter in_combat")?;

    for (cid, _side, init) in &with_init {
        tx.execute(
            "UPDATE encounter_participants
             SET initiative = ?1, has_acted_this_round = 0
             WHERE encounter_id = ?2 AND character_id = ?3",
            params![*init, p.encounter_id, *cid],
        )
        .context("update participant initiative")?;
    }

    let initiative_order: Vec<InitiativeEntry> = with_init
        .iter()
        .map(|(cid, side, init)| InitiativeEntry {
            character_id: *cid,
            side: side.clone(),
            initiative: *init,
        })
        .collect();

    let event_participants: Vec<Participant<'_>> = with_init
        .iter()
        .map(|(cid, _, _)| Participant {
            character_id: *cid,
            role: "actor",
        })
        .collect();

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "combat.start",
            campaign_hour: now,
            combat_round: Some(1),
            zone_id: row.zone_id,
            encounter_id: Some(p.encounter_id),
            parent_id: None,
            summary: format!(
                "Combat started on encounter id={} with {n} participant(s)",
                p.encounter_id,
                n = initiative_order.len()
            ),
            payload: serde_json::json!({
                "initiative_order": initiative_order,
                "auto_ended_encounter_id": auto_ended_id,
            }),
            participants: &event_participants,
            items: &[],
        },
    )?;

    tx.commit().context("commit combat.start tx")?;

    Ok(StartResult {
        encounter_id: p.encounter_id,
        current_round: 1,
        initiative_order,
        auto_ended_encounter_id: auto_ended_id,
        event_id: emitted.event_id,
    })
}

// ── combat.next_turn ─────────────────────────────────────────────────────────

pub fn next_turn(conn: &mut Connection, p: NextTurnParams) -> Result<NextTurnResult> {
    let row = read_encounter_or_404(conn, p.encounter_id)?;
    if !row.in_combat {
        bail!("encounter {} is not in combat", p.encounter_id);
    }
    let current_turn = row.turn_index.unwrap_or(0);
    let current_round = row.current_round.unwrap_or(1);

    // Read participants in initiative order.
    let mut stmt = conn.prepare(
        "SELECT character_id FROM encounter_participants
         WHERE encounter_id = ?1
         ORDER BY initiative DESC, character_id ASC",
    )?;
    let ordered: Vec<i64> = stmt
        .query_map([p.encounter_id], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    if ordered.is_empty() {
        bail!("encounter {} has no participants", p.encounter_id);
    }
    let n = ordered.len() as i32;

    // Clamp a stale turn_index — a participant added/removed between turns could leave
    // the stored index outside the current initiative list, and `ordered[...]` would
    // panic the server thread. Treat out-of-range as "start of next round".
    let current_turn = current_turn.clamp(0, n - 1);
    let next_turn_idx = current_turn + 1;
    let (new_turn, new_round, wrapped) = if next_turn_idx >= n {
        (0, current_round + 1, true)
    } else {
        (next_turn_idx, current_round, false)
    };
    let current_actor = ordered[current_turn as usize];
    let next_actor = ordered[new_turn as usize];
    let now = crate::world::current_campaign_hour(conn)?;

    let tx = conn.transaction().context("begin combat.next_turn tx")?;

    // Mark the currently-acting participant as having acted.
    tx.execute(
        "UPDATE encounter_participants
         SET has_acted_this_round = 1
         WHERE encounter_id = ?1 AND character_id = ?2",
        params![p.encounter_id, current_actor],
    )
    .context("mark participant acted")?;

    let mut expired_effect_ids: Vec<i64> = Vec::new();
    let mut expired_condition_ids: Vec<i64> = Vec::new();
    if wrapped {
        // Reset acted flags for everyone.
        tx.execute(
            "UPDATE encounter_participants
             SET has_acted_this_round = 0
             WHERE encounter_id = ?1",
            [p.encounter_id],
        )
        .context("reset acted flags")?;

        // Decrement round-based timers for effects on any participant of this encounter.
        // Anything hitting zero is flipped active=0 and given an effect.expired event.
        let char_ids_csv: String = ordered
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");

        // Effects.
        let effect_sql = format!(
            "SELECT id, target_character_id, source, target_kind, target_key, expires_after_rounds
             FROM effects
             WHERE active = 1
               AND expires_after_rounds IS NOT NULL
               AND target_character_id IN ({char_ids_csv})"
        );
        let mut es = tx.prepare(&effect_sql)?;
        let effects: Vec<(i64, i64, String, String, String, i32)> = es
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(es);
        for (eid, target, source, tk, tkey, rounds) in effects {
            let new_rounds = rounds - 1;
            if new_rounds <= 0 {
                tx.execute("UPDATE effects SET active = 0 WHERE id = ?1", [eid])?;
                let emitted = events::emit_in_tx(
                    &tx,
                    &EventSpec {
                        kind: "effect.expired",
                        campaign_hour: now,
                        combat_round: Some(new_round),
                        zone_id: row.zone_id,
                        encounter_id: Some(p.encounter_id),
                        parent_id: None,
                        summary: format!(
                            "Effect {source} on character id={target} expired at combat round {new_round} ({tk}:{tkey})"
                        ),
                        payload: serde_json::json!({
                            "effect_id": eid,
                            "source": source,
                            "target_kind": tk,
                            "target_key": tkey,
                            "reason": "rounds_elapsed",
                            "expiry_reason": "rounds_elapsed",
                        }),
                        participants: &[Participant {
                            character_id: target,
                            role: "target",
                        }],
                        items: &[],
                    },
                )?;
                let _ = emitted;
                expired_effect_ids.push(eid);
            } else {
                tx.execute(
                    "UPDATE effects SET expires_after_rounds = ?1 WHERE id = ?2",
                    params![new_rounds, eid],
                )?;
            }
        }

        // Conditions.
        let cond_sql = format!(
            "SELECT id, character_id, condition, severity, expires_after_rounds
             FROM character_conditions
             WHERE active = 1
               AND expires_after_rounds IS NOT NULL
               AND character_id IN ({char_ids_csv})"
        );
        let mut cs = tx.prepare(&cond_sql)?;
        let conds: Vec<(i64, i64, String, i32, i32)> = cs
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(cs);
        for (cid, char_id, name, severity, rounds) in conds {
            let new_rounds = rounds - 1;
            if new_rounds <= 0 {
                tx.execute(
                    "UPDATE character_conditions SET active = 0 WHERE id = ?1",
                    [cid],
                )?;
                events::emit_in_tx(
                    &tx,
                    &EventSpec {
                        kind: "condition.expired",
                        campaign_hour: now,
                        combat_round: Some(new_round),
                        zone_id: row.zone_id,
                        encounter_id: Some(p.encounter_id),
                        parent_id: None,
                        summary: format!(
                            "Condition {name:?} on character id={char_id} expired at combat round {new_round}"
                        ),
                        payload: serde_json::json!({
                            "condition_id": cid,
                            "condition": name,
                            "severity": severity,
                            "reason": "rounds_elapsed",
                        }),
                        participants: &[Participant {
                            character_id: char_id,
                            role: "target",
                        }],
                        items: &[],
                    },
                )?;
                expired_condition_ids.push(cid);
            } else {
                tx.execute(
                    "UPDATE character_conditions SET expires_after_rounds = ?1 WHERE id = ?2",
                    params![new_rounds, cid],
                )?;
            }
        }
    }

    tx.execute(
        "UPDATE encounters SET turn_index = ?1, current_round = ?2 WHERE id = ?3",
        params![new_turn, new_round, p.encounter_id],
    )
    .context("advance encounter turn/round")?;

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "combat.next_turn",
            campaign_hour: now,
            combat_round: Some(new_round),
            zone_id: row.zone_id,
            encounter_id: Some(p.encounter_id),
            parent_id: None,
            summary: format!(
                "Combat next turn on encounter id={} (round {new_round}, actor id={next_actor})",
                p.encounter_id
            ),
            payload: serde_json::json!({
                "current_round": new_round,
                "current_character_id": next_actor,
                "wrapped_to_new_round": wrapped,
                "expired_effect_ids": expired_effect_ids,
                "expired_condition_ids": expired_condition_ids,
            }),
            participants: &[Participant {
                character_id: next_actor,
                role: "actor",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit combat.next_turn tx")?;

    Ok(NextTurnResult {
        encounter_id: p.encounter_id,
        current_round: new_round,
        current_character_id: next_actor,
        wrapped_to_new_round: wrapped,
        expired_effect_ids,
        expired_condition_ids,
        event_id: emitted.event_id,
    })
}

// ── combat.end ───────────────────────────────────────────────────────────────

pub fn end(conn: &mut Connection, p: EndParams) -> Result<EndResult> {
    let row = read_encounter_or_404(conn, p.encounter_id)?;
    if !row.in_combat {
        bail!("encounter {} is not in combat", p.encounter_id);
    }
    let now = crate::world::current_campaign_hour(conn)?;
    let tx = conn.transaction().context("begin combat.end tx")?;
    let reason = p.reason.as_deref().unwrap_or("combat_ended");
    let event_id = end_combat_in_tx(&tx, p.encounter_id, now, reason, false)?;
    tx.commit().context("commit combat.end tx")?;
    Ok(EndResult {
        encounter_id: p.encounter_id,
        event_id,
    })
}

/// Shared by explicit `combat.end` and `combat.start`'s auto-cleanup path. Emits
/// `combat.auto_ended` when `auto = true`, `combat.end` otherwise.
fn end_combat_in_tx(
    tx: &rusqlite::Transaction<'_>,
    encounter_id: i64,
    now: i64,
    reason: &str,
    auto: bool,
) -> Result<i64> {
    // Capture zone_id for the event.
    let (zone_id, current_round): (Option<i64>, Option<i32>) = tx
        .query_row(
            "SELECT zone_id, current_round FROM encounters WHERE id = ?1",
            [encounter_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .with_context(|| format!("read encounter {encounter_id}"))?;

    tx.execute(
        "UPDATE encounters
         SET in_combat = 0, current_round = NULL, turn_index = NULL
         WHERE id = ?1",
        [encounter_id],
    )
    .context("unflag encounter in_combat")?;
    tx.execute(
        "UPDATE encounter_participants
         SET initiative = NULL, has_acted_this_round = 0
         WHERE encounter_id = ?1",
        [encounter_id],
    )
    .context("null participant combat fields")?;

    let kind = if auto {
        "combat.auto_ended"
    } else {
        "combat.end"
    };
    let emitted = events::emit_in_tx(
        tx,
        &EventSpec {
            kind,
            campaign_hour: now,
            combat_round: current_round,
            zone_id,
            encounter_id: Some(encounter_id),
            parent_id: None,
            summary: format!("Combat on encounter id={encounter_id} ended ({reason})"),
            payload: serde_json::json!({
                "reason": reason,
                "auto": auto,
            }),
            participants: &[],
            items: &[],
        },
    )?;
    Ok(emitted.event_id)
}

// ── combat.apply_damage / combat.apply_healing ───────────────────────────────

pub fn apply_damage(conn: &mut Connection, p: ApplyDamageParams) -> Result<ApplyDamageResult> {
    if p.amount < 0 {
        bail!("damage amount must be non-negative (use apply_healing for healing)");
    }

    let (hp_current, hp_max, status): (i32, i32, String) = conn
        .query_row(
            "SELECT hp_current, hp_max, status FROM characters WHERE id = ?1",
            [p.character_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .with_context(|| format!("character {} not found", p.character_id))?;

    let new_hp = (hp_current - p.amount).max(0);
    let hits_zero = hp_current > 0 && new_hp == 0;
    let now = crate::world::current_campaign_hour(conn)?;
    let combat_round: Option<i32> = match p.encounter_id {
        None => None,
        Some(eid) => conn
            .query_row(
                "SELECT current_round FROM encounters WHERE id = ?1",
                [eid],
                |row| row.get::<_, Option<i32>>(0),
            )
            .optional_ok()?
            .flatten(),
    };

    let tx = conn.transaction().context("begin apply_damage tx")?;

    let (new_status, mortally_wounded_id) = if hits_zero && status == "alive" {
        // Drop unconscious + apply mortally_wounded condition in the same tx.
        tx.execute(
            "UPDATE characters
             SET hp_current = 0, status = 'unconscious',
                 death_save_successes = 0, death_save_failures = 0,
                 updated_at = ?1
             WHERE id = ?2",
            params![now, p.character_id],
        )
        .context("set unconscious")?;

        let cond_event = events::emit_in_tx(
            &tx,
            &EventSpec {
                kind: "condition.applied",
                campaign_hour: now,
                combat_round,
                zone_id: None,
                encounter_id: p.encounter_id,
                parent_id: None,
                summary: format!("Character id={} mortally_wounded at 0 HP", p.character_id),
                payload: serde_json::json!({
                    "condition": "mortally_wounded",
                    "severity": 1,
                }),
                participants: &[Participant {
                    character_id: p.character_id,
                    role: "target",
                }],
                items: &[],
            },
        )?;
        tx.execute(
            "INSERT INTO character_conditions
                (character_id, condition, severity, source_event_id, active)
             VALUES (?1, 'mortally_wounded', 1, ?2, 1)",
            params![p.character_id, cond_event.event_id],
        )
        .context("insert mortally_wounded condition row")?;
        let cond_id = tx.last_insert_rowid();
        ("unconscious".to_string(), Some(cond_id))
    } else {
        tx.execute(
            "UPDATE characters SET hp_current = ?1, updated_at = ?2 WHERE id = ?3",
            params![new_hp, now, p.character_id],
        )
        .context("set hp_current")?;
        (status.clone(), None)
    };

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "combat.apply_damage",
            campaign_hour: now,
            combat_round,
            zone_id: None,
            encounter_id: p.encounter_id,
            parent_id: None,
            summary: format!(
                "Character id={} took {} damage ({hp_current}→{new_hp} HP)",
                p.character_id, p.amount
            ),
            payload: serde_json::json!({
                "amount": p.amount,
                "damage_type": p.damage_type,
                "source": p.source,
                "hp_before": hp_current,
                "hp_after": new_hp,
                "newly_unconscious": hits_zero,
                "hp_max": hp_max,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit apply_damage tx")?;

    Ok(ApplyDamageResult {
        character_id: p.character_id,
        hp_current: new_hp,
        status: new_status,
        newly_unconscious: hits_zero,
        mortally_wounded_condition_id: mortally_wounded_id,
        event_id: emitted.event_id,
    })
}

pub fn apply_healing(conn: &mut Connection, p: ApplyHealingParams) -> Result<ApplyHealingResult> {
    if p.amount < 0 {
        bail!("healing amount must be non-negative");
    }
    let (hp_current, hp_max, status): (i32, i32, String) = conn
        .query_row(
            "SELECT hp_current, hp_max, status FROM characters WHERE id = ?1",
            [p.character_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .with_context(|| format!("character {} not found", p.character_id))?;
    if status == "dead" {
        bail!("character {} is dead and cannot be healed", p.character_id);
    }
    let new_hp = (hp_current + p.amount).min(hp_max);
    let now = crate::world::current_campaign_hour(conn)?;

    let tx = conn.transaction().context("begin apply_healing tx")?;

    // If mortally_wounded active and we're going above 0, clear the condition + revive.
    let mw_row: Option<i64> = tx
        .query_row(
            "SELECT id FROM character_conditions
             WHERE character_id = ?1 AND condition = 'mortally_wounded' AND active = 1
             ORDER BY id DESC LIMIT 1",
            [p.character_id],
            |row| row.get(0),
        )
        .optional_ok()?;

    let mut cleared_mw = false;
    let new_status = if let (true, Some(mw_id)) = (new_hp > 0 && status == "unconscious", mw_row) {
        tx.execute(
            "UPDATE character_conditions SET active = 0 WHERE id = ?1",
            [mw_id],
        )?;
        events::emit_in_tx(
            &tx,
            &EventSpec {
                kind: "condition.expired",
                campaign_hour: now,
                combat_round: None,
                zone_id: None,
                encounter_id: None,
                parent_id: None,
                summary: format!(
                    "Character id={} recovered: mortally_wounded cleared by healing",
                    p.character_id
                ),
                payload: serde_json::json!({
                    "condition_id": mw_id,
                    "condition": "mortally_wounded",
                    "reason": "healed_above_zero",
                }),
                participants: &[Participant {
                    character_id: p.character_id,
                    role: "target",
                }],
                items: &[],
            },
        )?;
        cleared_mw = true;
        "alive".to_string()
    } else {
        status.clone()
    };

    tx.execute(
        "UPDATE characters
         SET hp_current = ?1, status = ?2,
             death_save_successes = CASE WHEN ?2 = 'alive' THEN 0 ELSE death_save_successes END,
             death_save_failures  = CASE WHEN ?2 = 'alive' THEN 0 ELSE death_save_failures END,
             updated_at = ?3
         WHERE id = ?4",
        params![new_hp, new_status, now, p.character_id],
    )
    .context("apply healing")?;

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "combat.apply_healing",
            campaign_hour: now,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={} healed by {} ({hp_current}→{new_hp} HP)",
                p.character_id, p.amount
            ),
            payload: serde_json::json!({
                "amount": p.amount,
                "source": p.source,
                "hp_before": hp_current,
                "hp_after": new_hp,
                "cleared_mortally_wounded": cleared_mw,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit apply_healing tx")?;

    Ok(ApplyHealingResult {
        character_id: p.character_id,
        hp_current: new_hp,
        status: new_status,
        cleared_mortally_wounded: cleared_mw,
        event_id: emitted.event_id,
    })
}

// ── Death flow ───────────────────────────────────────────────────────────────

pub fn roll_death_save(conn: &mut Connection, p: DeathSaveParams) -> Result<DeathSaveResult> {
    let (status, successes, failures): (String, i32, i32) = conn
        .query_row(
            "SELECT status, death_save_successes, death_save_failures
             FROM characters WHERE id = ?1",
            [p.character_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .with_context(|| format!("character {} not found", p.character_id))?;
    if status != "unconscious" {
        bail!(
            "death saves only apply to unconscious characters (id={} is {:?})",
            p.character_id,
            status
        );
    }

    let mut rng = rand::rng();
    let d20: i32 = rng.random_range(1..=20);

    let (mut new_successes, mut new_failures, outcome) = if d20 == 20 {
        // Auto-stabilise: reset counters, heal to 1, status alive.
        (0, 0, "auto_stabilise".to_string())
    } else if d20 == 1 {
        (
            successes,
            (failures + 2).min(3),
            "critical_fail".to_string(),
        )
    } else if d20 >= 10 {
        ((successes + 1).min(3), failures, "success".to_string())
    } else {
        (successes, (failures + 1).min(3), "fail".to_string())
    };

    let mut new_status = status.clone();
    let mut narrative_extra = "";
    if outcome == "auto_stabilise" || new_successes >= 3 {
        new_status = "alive".to_string();
        new_successes = 0;
        new_failures = 0;
        narrative_extra = " (stabilised)";
    } else if new_failures >= 3 {
        new_status = "dead".to_string();
        narrative_extra = " (died)";
    }

    let now = crate::world::current_campaign_hour(conn)?;

    let tx = conn.transaction().context("begin roll_death_save tx")?;

    // Per docs/characters.md §Death: both stabilise paths (nat-20 auto and 3 successes)
    // set hp_current=1 and clear counters. Without this, a 3-success stabilise leaves
    // hp_current at 0 with status='alive', and a following apply_damage can't retrigger
    // the death flow (hits_zero requires hp_current > 0 before the strike).
    let hp_update_sql = if new_status == "alive" {
        "UPDATE characters
         SET status = ?1, death_save_successes = ?2, death_save_failures = ?3,
             hp_current = MAX(hp_current, 1), updated_at = ?4
         WHERE id = ?5"
    } else {
        "UPDATE characters
         SET status = ?1, death_save_successes = ?2, death_save_failures = ?3,
             updated_at = ?4
         WHERE id = ?5"
    };
    tx.execute(
        hp_update_sql,
        params![new_status, new_successes, new_failures, now, p.character_id,],
    )
    .context("update death save counters")?;

    // If we just stabilised, clear the mortally_wounded condition and emit the matching
    // condition.expired event so consumers tailing the event stream see the transition
    // (apply_healing emits the same event on its equivalent path).
    if new_status == "alive" {
        if let Some(mw_id) = tx
            .query_row(
                "SELECT id FROM character_conditions
                 WHERE character_id = ?1 AND condition = 'mortally_wounded' AND active = 1
                 ORDER BY id DESC LIMIT 1",
                [p.character_id],
                |row| row.get::<_, i64>(0),
            )
            .optional_ok()?
        {
            tx.execute(
                "UPDATE character_conditions SET active = 0 WHERE id = ?1",
                [mw_id],
            )?;
            events::emit_in_tx(
                &tx,
                &EventSpec {
                    kind: "condition.expired",
                    campaign_hour: now,
                    combat_round: None,
                    zone_id: None,
                    encounter_id: None,
                    parent_id: None,
                    summary: format!(
                        "Character id={} stabilised: mortally_wounded cleared ({outcome})",
                        p.character_id
                    ),
                    payload: serde_json::json!({
                        "condition_id": mw_id,
                        "condition": "mortally_wounded",
                        "reason": "stabilised",
                        "outcome": outcome,
                    }),
                    participants: &[Participant {
                        character_id: p.character_id,
                        role: "target",
                    }],
                    items: &[],
                },
            )?;
        }
    }

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "character.death_save",
            campaign_hour: now,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={} rolled {d20} on death save → {outcome}{narrative_extra} ({new_successes} successes / {new_failures} failures)",
                p.character_id
            ),
            payload: serde_json::json!({
                "d20": d20,
                "outcome": outcome,
                "successes": new_successes,
                "failures": new_failures,
                "status_before": status,
                "status_after": new_status,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "actor",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit roll_death_save tx")?;

    Ok(DeathSaveResult {
        character_id: p.character_id,
        d20,
        outcome,
        successes: new_successes,
        failures: new_failures,
        status: new_status,
        event_id: emitted.event_id,
    })
}

pub fn roll_death_event(
    conn: &mut Connection,
    content: &Content,
    p: DeathEventParams,
) -> Result<DeathEventResult> {
    let status: String = conn
        .query_row(
            "SELECT status FROM characters WHERE id = ?1",
            [p.character_id],
            |row| row.get(0),
        )
        .with_context(|| format!("character {} not found", p.character_id))?;
    if status != "dead" {
        bail!(
            "roll_death_event requires status='dead' (character {} is {:?})",
            p.character_id,
            status
        );
    }
    if content.death_events.is_empty() {
        bail!("death_events content table is empty");
    }

    let total_weight: i32 = content.death_events.iter().map(|e| e.weight.max(1)).sum();
    let mut rng = rand::rng();
    let mut roll = rng.random_range(1..=total_weight);
    let chosen = content
        .death_events
        .iter()
        .find(|e| {
            let w = e.weight.max(1);
            if roll <= w {
                true
            } else {
                roll -= w;
                false
            }
        })
        .cloned()
        .unwrap_or_else(|| content.death_events[0].clone());

    let now = crate::world::current_campaign_hour(conn)?;
    let event = events::emit(
        conn,
        &EventSpec {
            kind: "character.death_event",
            campaign_hour: now,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={} rolled death event {:?}",
                p.character_id, chosen.kind
            ),
            payload: serde_json::json!({
                "kind": chosen.kind,
                "description": chosen.description,
                "outcome_hooks": chosen.outcome_hooks,
                "requires": chosen.requires,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "actor",
            }],
            items: &[],
        },
    )?;

    Ok(DeathEventResult {
        character_id: p.character_id,
        rolled: chosen,
        event_id: event.event_id,
    })
}

// ── Optional-row helper (same shape as elsewhere) ────────────────────────────

trait OptionalOk<T> {
    fn optional_ok(self) -> Result<Option<T>>;
}

impl<T> OptionalOk<T> for rusqlite::Result<T> {
    fn optional_ok(self) -> Result<Option<T>> {
        match self {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("query_row"),
        }
    }
}

// Unused imports dodge — `EncounterRow` helps the signature but is otherwise
// unused outside `start`/`next_turn`/`end` which read the row directly.
#[allow(dead_code)]
fn _keep_encounter_row_import(row: EncounterRow) -> EncounterRow {
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::characters::{self, CreateParams as CharCreateParams};
    use crate::db::schema;
    use crate::effects;
    use crate::encounters::{self as enc, CreateParams as EncCreate, CreateParticipant};

    fn fresh() -> (Connection, Content) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        (conn, Content::load(None).unwrap())
    }

    fn char(conn: &mut Connection, name: &str, role: &str, con_score: i32, hp: i32) -> i64 {
        characters::create(
            conn,
            CharCreateParams {
                name: name.into(),
                role: role.into(),
                str_score: 10,
                dex_score: 10,
                con_score,
                int_score: 10,
                wis_score: 10,
                cha_score: 10,
                hp_max: Some(hp),
                hp_current: Some(hp),
                armor_class: Some(12),
                speed_ft: None,
                initiative_bonus: Some(1),
                size: None,
                species: None,
                class_or_archetype: None,
                ideology: None,
                backstory: None,
                plans: None,
                loyalty: None,
                party_id: None,
                current_zone_id: None,
            },
        )
        .unwrap()
        .character_id
    }

    fn simple_encounter(conn: &mut Connection, players: &[i64], enemies: &[i64]) -> i64 {
        let participants = players
            .iter()
            .map(|id| CreateParticipant {
                character_id: *id,
                side: "player_side".into(),
            })
            .chain(enemies.iter().map(|id| CreateParticipant {
                character_id: *id,
                side: "hostile".into(),
            }))
            .collect();
        enc::create(
            conn,
            EncCreate {
                zone_id: None,
                name: Some("test".into()),
                goal: "g".into(),
                estimated_duration_hours: Some(1),
                xp_budget: 100,
                participants,
            },
        )
        .unwrap()
        .encounter_id
    }

    #[test]
    fn round_based_effect_expires_on_wrap() {
        let (mut conn, _content) = fresh();
        let player = char(&mut conn, "P", "player", 12, 20);
        let enemy = char(&mut conn, "E", "enemy", 12, 10);
        let eid = simple_encounter(&mut conn, &[player], &[enemy]);
        start(&mut conn, StartParams { encounter_id: eid }).unwrap();

        // 2-round effect on the player.
        let applied = effects::apply(
            &mut conn,
            effects::ApplyParams {
                target_character_id: player,
                source: "spell:bless".into(),
                target_kind: "attack".into(),
                target_key: "attack".into(),
                modifier: 0,
                dice_expr: Some("1d4".into()),
                expires_at_hour: None,
                expires_after_rounds: Some(2),
                expires_on_dispel: Some(true),
            },
        )
        .unwrap();

        // 2 participants; next_turn pointer rolls over every 2 calls.
        // Round 1 → next_turn (enemy) → next_turn wraps to round 2 (-> effect ticks 2→1).
        next_turn(&mut conn, NextTurnParams { encounter_id: eid }).unwrap();
        let r = next_turn(&mut conn, NextTurnParams { encounter_id: eid }).unwrap();
        assert!(r.wrapped_to_new_round);
        assert_eq!(r.current_round, 2);
        // Effect still active.
        let active: i64 = conn
            .query_row(
                "SELECT active FROM effects WHERE id = ?1",
                [applied.effect_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 1);

        // Continue through round 2 into round 3 → effect now ticks 1→0 → inactive.
        next_turn(&mut conn, NextTurnParams { encounter_id: eid }).unwrap();
        let r = next_turn(&mut conn, NextTurnParams { encounter_id: eid }).unwrap();
        assert_eq!(r.current_round, 3);
        assert!(r.expired_effect_ids.contains(&applied.effect_id));
        let active: i64 = conn
            .query_row(
                "SELECT active FROM effects WHERE id = ?1",
                [applied.effect_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 0);
    }

    #[test]
    fn stale_combat_is_auto_ended_on_new_start() {
        let (mut conn, _content) = fresh();
        let player = char(&mut conn, "P", "player", 12, 20);
        let enemy1 = char(&mut conn, "E1", "enemy", 12, 10);
        let enemy2 = char(&mut conn, "E2", "enemy", 12, 10);
        let eid1 = simple_encounter(&mut conn, &[player], &[enemy1]);
        let eid2 = simple_encounter(&mut conn, &[player], &[enemy2]);

        start(&mut conn, StartParams { encounter_id: eid1 }).unwrap();
        let r2 = start(&mut conn, StartParams { encounter_id: eid2 }).unwrap();
        assert_eq!(r2.auto_ended_encounter_id, Some(eid1));

        let (in_combat, current_round): (i64, Option<i32>) = conn
            .query_row(
                "SELECT in_combat, current_round FROM encounters WHERE id = ?1",
                [eid1],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            in_combat, 0,
            "first encounter should have in_combat=0 after auto-end"
        );
        assert!(current_round.is_none());

        // combat.auto_ended event emitted.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind='combat.auto_ended' AND encounter_id=?1",
                [eid1],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn damage_to_zero_drops_unconscious_with_mortally_wounded() {
        let (mut conn, _content) = fresh();
        let player = char(&mut conn, "P", "player", 10, 10);
        let r = apply_damage(
            &mut conn,
            ApplyDamageParams {
                character_id: player,
                amount: 12,
                damage_type: Some("slashing".into()),
                source: Some("orc raider".into()),
                encounter_id: None,
            },
        )
        .unwrap();
        assert_eq!(r.hp_current, 0);
        assert!(r.newly_unconscious);
        assert_eq!(r.status, "unconscious");
        assert!(r.mortally_wounded_condition_id.is_some());
    }

    #[test]
    fn three_death_save_failures_marks_dead_and_rolls_death_event() {
        let (mut conn, content) = fresh();
        let player = char(&mut conn, "P", "player", 10, 10);
        apply_damage(
            &mut conn,
            ApplyDamageParams {
                character_id: player,
                amount: 100,
                damage_type: None,
                source: None,
                encounter_id: None,
            },
        )
        .unwrap();
        // Force three failures by setting the counter directly.
        conn.execute(
            "UPDATE characters SET death_save_failures = 2 WHERE id = ?1",
            [player],
        )
        .unwrap();
        // Keep rolling until we land a failure for the third point. Loop bounded to avoid
        // an infinite loop on an unlikely streak of successes.
        let mut attempts = 0;
        loop {
            attempts += 1;
            assert!(attempts < 200, "never produced third failure");
            let save = roll_death_save(
                &mut conn,
                DeathSaveParams {
                    character_id: player,
                },
            )
            .unwrap();
            if save.status == "dead" {
                break;
            }
            // If the save succeeded or auto-stabilised, restore the pre-roll failure count
            // so we can try again.
            conn.execute(
                "UPDATE characters SET death_save_failures = 2, death_save_successes = 0,
                                         status = 'unconscious'
                 WHERE id = ?1",
                [player],
            )
            .unwrap();
        }
        let de = roll_death_event(
            &mut conn,
            &content,
            DeathEventParams {
                character_id: player,
            },
        )
        .unwrap();
        assert!(!de.rolled.kind.is_empty());
    }
}
