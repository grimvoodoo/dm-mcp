//! Encounters: lifecycle tools (create/complete/abandon/fail).
//!
//! Combat state lives on the `encounters` row itself — see `crate::combat` for
//! combat.start/next_turn/end/apply_damage/apply_healing. This module handles the
//! narrative container only.
//!
//! Key decision (per `docs/encounters.md`): XP fires on `encounter.goal_completed` with
//! the encounter's full `xp_budget`, regardless of resolution path. Bypass earns the same
//! as combat victory. `xp_modifier` on complete() scales the award (e.g. 0.5 for a flight
//! that abandons the villagers).

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::events::{self, EventSpec, Participant};

// ── Params / results ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CreateParticipant {
    pub character_id: i64,
    /// `player_side`, `hostile`, `neutral`, or `ally`.
    pub side: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CreateParams {
    #[serde(default)]
    pub zone_id: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    pub goal: String,
    #[serde(default)]
    pub estimated_duration_hours: Option<i32>,
    pub xp_budget: i32,
    /// Characters drawn into this encounter, with their side. Empty is allowed — the DM
    /// may add participants later via `encounter.add_participant` (deferred tool).
    #[serde(default)]
    pub participants: Vec<CreateParticipant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateResult {
    pub encounter_id: i64,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CompleteParams {
    pub encounter_id: i64,
    /// Resolution path label (free-text per `docs/encounters.md`). Recorded on the event.
    pub path: String,
    /// Multiplier on xp_budget (default 1.0). Clamped to [0.0, 2.0].
    #[serde(default)]
    pub xp_modifier: Option<f64>,
    /// In-world hours elapsed during the encounter. Defaults to the encounter's
    /// `estimated_duration_hours`.
    #[serde(default)]
    pub hours_elapsed: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompleteResult {
    pub encounter_id: i64,
    pub status: String,
    pub xp_awarded_total: i32,
    pub per_player_xp: Vec<PlayerXpAward>,
    pub event_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerXpAward {
    pub character_id: i64,
    pub xp: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AbandonParams {
    pub encounter_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FailParams {
    pub encounter_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutcomeResult {
    pub encounter_id: i64,
    pub status: String,
    pub event_id: i64,
}

// ── Valid side / status sets ─────────────────────────────────────────────────

pub const VALID_SIDES: &[&str] = &["player_side", "hostile", "neutral", "ally"];

// ── Implementation ───────────────────────────────────────────────────────────

pub fn create(conn: &mut Connection, p: CreateParams) -> Result<CreateResult> {
    // XP budget is awarded to player_side participants on encounter.complete; allowing
    // a negative value would let a malicious or buggy caller deduct XP from players,
    // which has no game-rules basis. Reject up front rather than letting it propagate.
    if p.xp_budget < 0 {
        bail!("encounter xp_budget must be >= 0 (got {})", p.xp_budget);
    }
    for part in &p.participants {
        if !VALID_SIDES.contains(&part.side.as_str()) {
            bail!(
                "participant {} has unknown side {:?}; valid: {VALID_SIDES:?}",
                part.character_id,
                part.side
            );
        }
    }

    let started_at = crate::world::current_campaign_hour(conn)?;

    let tx = conn.transaction().context("begin encounter.create tx")?;
    tx.execute(
        "INSERT INTO encounters (
            zone_id, name, goal, estimated_duration_hours, xp_budget,
            status, in_combat, started_at_hour
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', 0, ?6)",
        params![
            p.zone_id,
            p.name,
            p.goal,
            p.estimated_duration_hours.unwrap_or(0),
            p.xp_budget,
            started_at,
        ],
    )
    .context("insert encounters row")?;
    let encounter_id = tx.last_insert_rowid();

    for part in &p.participants {
        tx.execute(
            "INSERT INTO encounter_participants
                (encounter_id, character_id, side, has_acted_this_round)
             VALUES (?1, ?2, ?3, 0)",
            params![encounter_id, part.character_id, part.side],
        )
        .with_context(|| {
            format!(
                "insert encounter_participants (enc={encounter_id}, char={}, side={:?})",
                part.character_id, part.side
            )
        })?;
    }

    let event_participants: Vec<Participant<'_>> = p
        .participants
        .iter()
        .map(|cp| Participant {
            character_id: cp.character_id,
            role: if cp.side == "player_side" {
                "actor"
            } else {
                "target"
            },
        })
        .collect();

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "encounter.created",
            campaign_hour: started_at,
            combat_round: None,
            zone_id: p.zone_id,
            encounter_id: Some(encounter_id),
            parent_id: None,
            summary: format!(
                "Encounter {:?} created (id={encounter_id}, xp_budget={})",
                p.name.as_deref().unwrap_or(&p.goal),
                p.xp_budget
            ),
            payload: serde_json::json!({
                "goal": p.goal,
                "xp_budget": p.xp_budget,
                "estimated_duration_hours": p.estimated_duration_hours,
                "participants": p.participants,
            }),
            participants: &event_participants,
            items: &[],
        },
    )?;

    tx.commit().context("commit encounter.create tx")?;

    Ok(CreateResult {
        encounter_id,
        event_id: emitted.event_id,
    })
}

pub fn complete(conn: &mut Connection, p: CompleteParams) -> Result<CompleteResult> {
    let row = read_encounter_or_404(conn, p.encounter_id)?;
    if row.status != "active" {
        bail!(
            "encounter {} is not active (currently {:?}) — cannot complete",
            p.encounter_id,
            row.status
        );
    }

    let modifier = p.xp_modifier.unwrap_or(1.0).clamp(0.0, 2.0);
    let xp_budget_scaled = ((row.xp_budget as f64) * modifier).round() as i32;
    let player_side: Vec<i64> = conn
        .prepare(
            "SELECT character_id FROM encounter_participants
             WHERE encounter_id = ?1 AND side = 'player_side'",
        )?
        .query_map([p.encounter_id], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let per_share = if player_side.is_empty() {
        0
    } else {
        xp_budget_scaled / player_side.len() as i32
    };
    // Report what was actually credited. Integer division can leave up to
    // `player_side.len() - 1` XP unawarded for budgets that don't divide evenly; keeping
    // `xp_awarded_total = per_share * n` means the response and event payload agree with
    // the sum of per_player_xp entries rather than overstating by the truncated remainder.
    let xp_total = per_share * player_side.len() as i32;

    let hours_elapsed = p.hours_elapsed.unwrap_or(row.estimated_duration_hours);
    let now = crate::world::current_campaign_hour(conn)? + hours_elapsed.max(0) as i64;

    let tx = conn.transaction().context("begin encounter.complete tx")?;
    tx.execute(
        "UPDATE encounters SET status = 'goal_completed', ended_at_hour = ?1
         WHERE id = ?2",
        params![now, p.encounter_id],
    )
    .context("update encounter status")?;

    let mut awards = Vec::with_capacity(player_side.len());
    for pc_id in &player_side {
        tx.execute(
            "UPDATE characters SET xp_total = xp_total + ?1, updated_at = ?2
             WHERE id = ?3",
            params![per_share, now, *pc_id],
        )
        .with_context(|| format!("award xp to character {pc_id}"))?;
        awards.push(PlayerXpAward {
            character_id: *pc_id,
            xp: per_share,
        });
    }

    let participants_for_event: Vec<Participant<'_>> = player_side
        .iter()
        .map(|cid| Participant {
            character_id: *cid,
            role: "beneficiary",
        })
        .collect();

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "encounter.goal_completed",
            campaign_hour: now,
            combat_round: None,
            zone_id: row.zone_id,
            encounter_id: Some(p.encounter_id),
            parent_id: None,
            summary: format!(
                "Encounter id={} goal completed via path {:?} — awarded {xp_total} XP across {n} player(s)",
                p.encounter_id,
                p.path,
                n = player_side.len()
            ),
            payload: serde_json::json!({
                "path": p.path,
                "xp_modifier": modifier,
                "xp_budget": row.xp_budget,
                "xp_budget_scaled": xp_budget_scaled,
                "xp_total": xp_total,
                "per_player_xp": awards,
                "hours_elapsed": hours_elapsed,
            }),
            participants: &participants_for_event,
            items: &[],
        },
    )?;

    tx.commit().context("commit encounter.complete tx")?;

    Ok(CompleteResult {
        encounter_id: p.encounter_id,
        status: "goal_completed".to_string(),
        xp_awarded_total: xp_total,
        per_player_xp: awards,
        event_id: emitted.event_id,
    })
}

pub fn abandon(conn: &mut Connection, p: AbandonParams) -> Result<OutcomeResult> {
    finish_without_xp(
        conn,
        p.encounter_id,
        "abandoned",
        "encounter.abandoned",
        &p.reason,
    )
}

pub fn fail(conn: &mut Connection, p: FailParams) -> Result<OutcomeResult> {
    finish_without_xp(
        conn,
        p.encounter_id,
        "failed",
        "encounter.failed",
        &p.reason,
    )
}

fn finish_without_xp(
    conn: &mut Connection,
    encounter_id: i64,
    new_status: &str,
    event_kind: &str,
    reason: &str,
) -> Result<OutcomeResult> {
    let row = read_encounter_or_404(conn, encounter_id)?;
    if row.status != "active" {
        bail!(
            "encounter {} is not active (currently {:?})",
            encounter_id,
            row.status
        );
    }

    let now = crate::world::current_campaign_hour(conn)?;
    let tx = conn.transaction().context("begin encounter outcome tx")?;
    tx.execute(
        "UPDATE encounters SET status = ?1, ended_at_hour = ?2 WHERE id = ?3",
        params![new_status, now, encounter_id],
    )
    .context("update encounter status")?;

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: event_kind,
            campaign_hour: now,
            combat_round: None,
            zone_id: row.zone_id,
            encounter_id: Some(encounter_id),
            parent_id: None,
            summary: format!("Encounter id={encounter_id} {new_status}: {reason}"),
            payload: serde_json::json!({
                "reason": reason,
                "status": new_status,
            }),
            participants: &[],
            items: &[],
        },
    )?;

    tx.commit().context("commit encounter outcome tx")?;

    Ok(OutcomeResult {
        encounter_id,
        status: new_status.to_string(),
        event_id: emitted.event_id,
    })
}

// ── Helpers (also used by src/combat.rs) ─────────────────────────────────────

pub(crate) struct EncounterRow {
    pub(crate) zone_id: Option<i64>,
    pub(crate) xp_budget: i32,
    pub(crate) estimated_duration_hours: i32,
    pub(crate) status: String,
    pub(crate) in_combat: bool,
    pub(crate) current_round: Option<i32>,
    pub(crate) turn_index: Option<i32>,
}

pub(crate) fn read_encounter_or_404(conn: &Connection, id: i64) -> Result<EncounterRow> {
    conn.query_row(
        "SELECT zone_id, xp_budget, estimated_duration_hours, status,
                in_combat, current_round, turn_index
         FROM encounters WHERE id = ?1",
        [id],
        |row| {
            Ok(EncounterRow {
                zone_id: row.get(0)?,
                xp_budget: row.get(1)?,
                estimated_duration_hours: row.get(2)?,
                status: row.get(3)?,
                in_combat: row.get::<_, i64>(4)? != 0,
                current_round: row.get(5)?,
                turn_index: row.get(6)?,
            })
        },
    )
    .with_context(|| format!("encounter {id} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::characters::{self, CreateParams as CharCreateParams};
    use crate::db::schema;

    fn fresh() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        conn
    }

    fn make_char(conn: &mut Connection, name: &str, role: &str) -> i64 {
        characters::create(
            conn,
            CharCreateParams {
                name: name.into(),
                role: role.into(),
                str_score: 10,
                dex_score: 10,
                con_score: 10,
                int_score: 10,
                wis_score: 10,
                cha_score: 10,
                hp_max: Some(10),
                hp_current: Some(10),
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

    #[test]
    fn create_complete_awards_xp_to_player_side() {
        let mut conn = fresh();
        let player = make_char(&mut conn, "Kira", "player");
        let ally = make_char(&mut conn, "Ally", "companion");
        let enemy = make_char(&mut conn, "Goblin", "enemy");

        let c = create(
            &mut conn,
            CreateParams {
                zone_id: None,
                name: Some("Goblin ambush".into()),
                goal: "Survive the ambush".into(),
                estimated_duration_hours: Some(1),
                xp_budget: 200,
                participants: vec![
                    CreateParticipant {
                        character_id: player,
                        side: "player_side".into(),
                    },
                    CreateParticipant {
                        character_id: ally,
                        side: "player_side".into(),
                    },
                    CreateParticipant {
                        character_id: enemy,
                        side: "hostile".into(),
                    },
                ],
            },
        )
        .expect("create");

        let r = complete(
            &mut conn,
            CompleteParams {
                encounter_id: c.encounter_id,
                path: "combat_victory".into(),
                xp_modifier: None,
                hours_elapsed: None,
            },
        )
        .expect("complete");

        assert_eq!(r.status, "goal_completed");
        assert_eq!(r.xp_awarded_total, 200);
        // Split 200 / 2 = 100 each.
        assert_eq!(r.per_player_xp.len(), 2);
        for award in &r.per_player_xp {
            assert_eq!(award.xp, 100);
        }
        // xp_total bumped on player-side characters; enemy left alone.
        let xp: i32 = conn
            .query_row(
                "SELECT xp_total FROM characters WHERE id = ?1",
                [player],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(xp, 100);
        let xp: i32 = conn
            .query_row(
                "SELECT xp_total FROM characters WHERE id = ?1",
                [enemy],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(xp, 0, "enemies are not player_side so no XP");
    }

    #[test]
    fn complete_rejects_non_active() {
        let mut conn = fresh();
        let player = make_char(&mut conn, "K", "player");
        let c = create(
            &mut conn,
            CreateParams {
                zone_id: None,
                name: None,
                goal: "g".into(),
                estimated_duration_hours: None,
                xp_budget: 50,
                participants: vec![CreateParticipant {
                    character_id: player,
                    side: "player_side".into(),
                }],
            },
        )
        .unwrap();
        abandon(
            &mut conn,
            AbandonParams {
                encounter_id: c.encounter_id,
                reason: "fled".into(),
            },
        )
        .unwrap();
        let err = complete(
            &mut conn,
            CompleteParams {
                encounter_id: c.encounter_id,
                path: "x".into(),
                xp_modifier: None,
                hours_elapsed: None,
            },
        )
        .expect_err("completing abandoned encounter should fail");
        assert!(format!("{err:#}").contains("not active"));
    }

    #[test]
    fn complete_reports_truthful_xp_total_under_integer_division() {
        // xp_budget 200, 3 player_side → per_share = 66, total credited = 198 (not 200).
        // The reported xp_awarded_total must match the sum of per_player_xp (regression
        // guard for CodeRabbit review on PR #12).
        let mut conn = fresh();
        let a = make_char(&mut conn, "A", "player");
        let b = make_char(&mut conn, "B", "companion");
        let c = make_char(&mut conn, "C", "companion");
        let enc = create(
            &mut conn,
            CreateParams {
                zone_id: None,
                name: None,
                goal: "g".into(),
                estimated_duration_hours: Some(1),
                xp_budget: 200,
                participants: vec![
                    CreateParticipant {
                        character_id: a,
                        side: "player_side".into(),
                    },
                    CreateParticipant {
                        character_id: b,
                        side: "player_side".into(),
                    },
                    CreateParticipant {
                        character_id: c,
                        side: "player_side".into(),
                    },
                ],
            },
        )
        .unwrap();
        let r = complete(
            &mut conn,
            CompleteParams {
                encounter_id: enc.encounter_id,
                path: "combat_victory".into(),
                xp_modifier: None,
                hours_elapsed: None,
            },
        )
        .unwrap();
        let per_player_sum: i32 = r.per_player_xp.iter().map(|a| a.xp).sum();
        assert_eq!(
            r.xp_awarded_total, per_player_sum,
            "xp_awarded_total must equal the sum of per_player_xp"
        );
        assert_eq!(r.xp_awarded_total, 198);
    }

    #[test]
    fn xp_modifier_scales_award() {
        let mut conn = fresh();
        let player = make_char(&mut conn, "K", "player");
        let c = create(
            &mut conn,
            CreateParams {
                zone_id: None,
                name: None,
                goal: "g".into(),
                estimated_duration_hours: None,
                xp_budget: 200,
                participants: vec![CreateParticipant {
                    character_id: player,
                    side: "player_side".into(),
                }],
            },
        )
        .unwrap();
        let r = complete(
            &mut conn,
            CompleteParams {
                encounter_id: c.encounter_id,
                path: "flight".into(),
                xp_modifier: Some(0.5),
                hours_elapsed: None,
            },
        )
        .unwrap();
        assert_eq!(r.xp_awarded_total, 100);
    }

    #[test]
    fn create_rejects_negative_xp_budget() {
        let mut conn = fresh();
        let err = create(
            &mut conn,
            CreateParams {
                zone_id: None,
                name: None,
                goal: "g".into(),
                estimated_duration_hours: None,
                xp_budget: -50,
                participants: vec![],
            },
        )
        .expect_err("negative xp_budget should bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("xp_budget") && msg.contains("-50"),
            "error should name the field and value: {msg}"
        );
    }
}
