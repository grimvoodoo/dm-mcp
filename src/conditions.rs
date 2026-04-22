//! Conditions: named states with mechanical riders (`blinded`, `poisoned`, `paralyzed`, …).
//!
//! See `docs/checks.md §Conditions`. Phase 5 surface:
//!
//! - [`apply`] inserts a `character_conditions` row and emits `condition.applied`.
//! - [`remove`] sets `active = 0` and emits `condition.expired`.
//!
//! The mechanical riders (disadvantage on attack rolls, auto-fail saves, etc.) live in
//! `content/rules/conditions.yaml` and are composed by `resolve_check` — this module only
//! manages the row lifecycle.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::events::{self, EventSpec, Participant};

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ApplyConditionParams {
    pub character_id: i64,
    /// Condition id matching a key in `content/rules/conditions.yaml` (e.g. `blinded`,
    /// `poisoned`, `mortally_wounded`). Phase 5 doesn't validate the name against content
    /// — misspellings would silently have no rider effect; later phases can tighten.
    pub condition: String,
    /// 1 for binary conditions; 1-6 for exhaustion.
    #[serde(default = "default_severity")]
    pub severity: i32,
    /// Optional — event id of whatever caused this condition (chained from a prior event).
    #[serde(default)]
    pub source_event_id: Option<i64>,
    /// Optional expiry at an in-game hour.
    #[serde(default)]
    pub expires_at_hour: Option<i64>,
    /// Optional round-based expiry (ticked by combat.next_turn in Phase 9).
    #[serde(default)]
    pub expires_after_rounds: Option<i32>,
    /// Optional save-on-retry spec (e.g. `save:con:dc15`).
    #[serde(default)]
    pub remove_on_save: Option<String>,
}

fn default_severity() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyConditionResult {
    pub condition_id: i64,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RemoveConditionParams {
    pub condition_id: i64,
    /// Free-text reason: "save succeeded", "spell dispelled", "time expired", etc.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoveConditionResult {
    pub condition_id: i64,
    pub event_id: i64,
}

/// Record a condition on a character and emit `condition.applied`.
pub fn apply(conn: &mut Connection, p: ApplyConditionParams) -> Result<ApplyConditionResult> {
    if p.condition.is_empty() {
        bail!("condition name must not be empty");
    }
    if p.severity < 1 {
        bail!("condition severity must be >= 1 (got {})", p.severity);
    }

    // Emit the event first so the source_event_id FK resolves once we write the row.
    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "condition.applied",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: p.source_event_id,
            summary: format!(
                "Applied condition {cond:?} to character id={char} (severity={sev})",
                cond = p.condition,
                char = p.character_id,
                sev = p.severity,
            ),
            payload: serde_json::json!({
                "condition": p.condition,
                "severity": p.severity,
                "expires_at_hour": p.expires_at_hour,
                "expires_after_rounds": p.expires_after_rounds,
                "remove_on_save": p.remove_on_save,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    conn.execute(
        "INSERT INTO character_conditions (
            character_id, condition, severity,
            source_event_id, expires_at_hour, expires_after_rounds, remove_on_save,
            active
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)",
        params![
            p.character_id,
            p.condition,
            p.severity,
            emitted.event_id,
            p.expires_at_hour,
            p.expires_after_rounds,
            p.remove_on_save,
        ],
    )
    .context("insert character_conditions row")?;

    Ok(ApplyConditionResult {
        condition_id: conn.last_insert_rowid(),
        event_id: emitted.event_id,
    })
}

/// Deactivate a condition and emit `condition.expired`. Refuses already-inactive conditions
/// so callers can't silently double-clear.
pub fn remove(conn: &mut Connection, p: RemoveConditionParams) -> Result<RemoveConditionResult> {
    let row: Option<(i64, String, i32, i64)> = conn
        .query_row(
            "SELECT character_id, condition, severity, active
             FROM character_conditions WHERE id = ?1",
            [p.condition_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional_ok()?;

    let (character_id, condition, severity, active) =
        row.ok_or_else(|| anyhow::anyhow!("condition {} not found", p.condition_id))?;

    if active == 0 {
        bail!("condition {} is already inactive", p.condition_id);
    }

    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "condition.expired",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Removed condition {condition:?} from character id={character_id} (severity={severity})",
            ),
            payload: serde_json::json!({
                "condition_id": p.condition_id,
                "condition": condition,
                "severity": severity,
                "reason": p.reason.clone().unwrap_or_else(|| "removed".to_string()),
            }),
            participants: &[Participant {
                character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    conn.execute(
        "UPDATE character_conditions SET active = 0 WHERE id = ?1",
        [p.condition_id],
    )
    .context("deactivate character_conditions row")?;

    Ok(RemoveConditionResult {
        condition_id: p.condition_id,
        event_id: emitted.event_id,
    })
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::characters::{self, CreateParams};
    use crate::db::schema;

    fn fresh_conn() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        conn
    }

    fn make_char(conn: &mut Connection) -> i64 {
        characters::create(
            conn,
            CreateParams {
                name: "K".into(),
                role: "player".into(),
                str_score: 10,
                dex_score: 10,
                con_score: 10,
                int_score: 10,
                wis_score: 10,
                cha_score: 10,
                hp_max: None,
                hp_current: None,
                armor_class: None,
                speed_ft: None,
                initiative_bonus: None,
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
    fn apply_and_remove_round_trip() {
        let mut conn = fresh_conn();
        let c = make_char(&mut conn);
        let applied = apply(
            &mut conn,
            ApplyConditionParams {
                character_id: c,
                condition: "blinded".into(),
                severity: 1,
                source_event_id: None,
                expires_at_hour: None,
                expires_after_rounds: None,
                remove_on_save: None,
            },
        )
        .unwrap();
        let active: i64 = conn
            .query_row(
                "SELECT active FROM character_conditions WHERE id = ?1",
                [applied.condition_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active, 1);

        remove(
            &mut conn,
            RemoveConditionParams {
                condition_id: applied.condition_id,
                reason: Some("ref test".into()),
            },
        )
        .unwrap();
        let active: i64 = conn
            .query_row(
                "SELECT active FROM character_conditions WHERE id = ?1",
                [applied.condition_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active, 0);
    }

    #[test]
    fn double_remove_errors() {
        let mut conn = fresh_conn();
        let c = make_char(&mut conn);
        let applied = apply(
            &mut conn,
            ApplyConditionParams {
                character_id: c,
                condition: "poisoned".into(),
                severity: 1,
                source_event_id: None,
                expires_at_hour: None,
                expires_after_rounds: None,
                remove_on_save: None,
            },
        )
        .unwrap();
        remove(
            &mut conn,
            RemoveConditionParams {
                condition_id: applied.condition_id,
                reason: None,
            },
        )
        .unwrap();
        let err = remove(
            &mut conn,
            RemoveConditionParams {
                condition_id: applied.condition_id,
                reason: None,
            },
        )
        .expect_err("double remove should fail");
        assert!(format!("{err:#}").contains("already inactive"));
    }
}
