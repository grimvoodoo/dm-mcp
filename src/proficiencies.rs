//! Proficiencies + resources CRUD.
//!
//! See `docs/checks.md` — the unified proficiencies table handles skills, saves
//! (`save:*`), weapons, tools, and custom growth skills under one shape. Resources are
//! limited-use counters (spell slots, mana, ki, hit dice, ...).
//!
//! Each mutating call emits an event so the log reflects the change, per the append-only
//! rule in `docs/history-log.md`.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::events::{self, EventSpec, Participant};

// ── Proficiencies ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SetProficiencyParams {
    pub character_id: i64,
    /// Name of the proficiency (skill, save, weapon, tool, or custom). See `docs/checks.md`.
    pub name: String,
    /// Is the character proficient? If false + ranks=0, the row is still inserted so the
    /// proficiency is reflected as "known, not proficient".
    #[serde(default)]
    pub proficient: Option<bool>,
    /// Expertise doubles the proficiency bonus (rogue class feature). Only meaningful when
    /// `proficient` is true.
    #[serde(default)]
    pub expertise: Option<bool>,
    /// Flat additional bonus. Used primarily for pet-style growth skills (e.g. `bite: 1`
    /// on a dog) that aren't part of the standard proficiency-bonus mechanic.
    #[serde(default)]
    pub ranks: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RemoveProficiencyParams {
    pub character_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProficiencyResult {
    pub character_id: i64,
    pub name: String,
    pub event_id: i64,
}

/// Upsert a row in `character_proficiencies` and emit `proficiency.set`.
pub fn set_proficiency(
    conn: &mut Connection,
    p: SetProficiencyParams,
) -> Result<ProficiencyResult> {
    if p.name.is_empty() {
        bail!("proficiency name must not be empty");
    }
    let proficient = p.proficient.unwrap_or(false);
    let expertise = p.expertise.unwrap_or(false);
    let ranks = p.ranks.unwrap_or(0);

    conn.execute(
        "INSERT INTO character_proficiencies (character_id, name, proficient, expertise, ranks)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(character_id, name) DO UPDATE SET
            proficient = excluded.proficient,
            expertise  = excluded.expertise,
            ranks      = excluded.ranks",
        params![
            p.character_id,
            p.name,
            i64::from(proficient),
            i64::from(expertise),
            ranks,
        ],
    )
    .context("upsert character_proficiencies")?;

    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "proficiency.set",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Proficiency {name:?} set on character id={char_id} (proficient={proficient}, expertise={expertise}, ranks={ranks})",
                name = p.name,
                char_id = p.character_id,
            ),
            payload: serde_json::json!({
                "name": p.name,
                "proficient": proficient,
                "expertise": expertise,
                "ranks": ranks,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    Ok(ProficiencyResult {
        character_id: p.character_id,
        name: p.name,
        event_id: emitted.event_id,
    })
}

/// Delete a proficiency row and emit `proficiency.removed`. Idempotent — deleting a
/// non-existent row is a no-op that still emits the event (useful for audit). Callers who
/// want strict semantics can `character.get` first to check.
pub fn remove_proficiency(
    conn: &mut Connection,
    p: RemoveProficiencyParams,
) -> Result<ProficiencyResult> {
    conn.execute(
        "DELETE FROM character_proficiencies WHERE character_id = ?1 AND name = ?2",
        params![p.character_id, p.name],
    )
    .context("delete character_proficiencies")?;

    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "proficiency.removed",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Proficiency {name:?} removed from character id={char_id}",
                name = p.name,
                char_id = p.character_id,
            ),
            payload: serde_json::json!({ "name": p.name }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    Ok(ProficiencyResult {
        character_id: p.character_id,
        name: p.name,
        event_id: emitted.event_id,
    })
}

// ── Resources ─────────────────────────────────────────────────────────────────

const VALID_RECHARGES: &[&str] = &["short_rest", "long_rest", "dawn", "never", "manual"];

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SetResourceParams {
    pub character_id: i64,
    /// Resource identifier, e.g. `slot:1`, `slot:9`, `hit_die`, `mana`, `ki`, `rage_use`.
    pub name: String,
    pub current: i32,
    pub max: i32,
    /// One of: `short_rest`, `long_rest`, `dawn`, `never`, `manual`.
    pub recharge: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AdjustResourceParams {
    pub character_id: i64,
    pub name: String,
    /// Positive to add, negative to subtract. Clamped to the `[0, max]` window of the
    /// existing row; returns an error if the resource doesn't exist yet.
    pub delta: i32,
    /// Free-text audit note (recorded on the event payload).
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RemoveResourceParams {
    pub character_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceResult {
    pub character_id: i64,
    pub name: String,
    pub current: i32,
    pub max: i32,
    pub event_id: i64,
}

/// Upsert a row in `character_resources`. Emits `resource.set`.
pub fn set_resource(conn: &mut Connection, p: SetResourceParams) -> Result<ResourceResult> {
    if p.name.is_empty() {
        bail!("resource name must not be empty");
    }
    if p.max < 0 {
        bail!("resource max must be >= 0");
    }
    if p.current < 0 || p.current > p.max {
        bail!(
            "resource current ({}) must be within [0, {}]",
            p.current,
            p.max
        );
    }
    if !VALID_RECHARGES.contains(&p.recharge.as_str()) {
        bail!(
            "unknown recharge {:?}; valid: {VALID_RECHARGES:?}",
            p.recharge
        );
    }

    conn.execute(
        "INSERT INTO character_resources (character_id, name, current, max, recharge)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(character_id, name) DO UPDATE SET
            current = excluded.current,
            max     = excluded.max,
            recharge= excluded.recharge",
        params![p.character_id, p.name, p.current, p.max, p.recharge],
    )
    .context("upsert character_resources")?;

    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "resource.set",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Resource {name:?} set on character id={char_id} ({current}/{max}, {recharge})",
                name = p.name,
                char_id = p.character_id,
                current = p.current,
                max = p.max,
                recharge = p.recharge,
            ),
            payload: serde_json::json!({
                "name": p.name,
                "current": p.current,
                "max": p.max,
                "recharge": p.recharge,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    Ok(ResourceResult {
        character_id: p.character_id,
        name: p.name,
        current: p.current,
        max: p.max,
        event_id: emitted.event_id,
    })
}

/// Adjust an existing resource by a signed delta. Emits `resource.adjusted`.
pub fn adjust_resource(conn: &mut Connection, p: AdjustResourceParams) -> Result<ResourceResult> {
    let row: Option<(i32, i32)> = conn
        .query_row(
            "SELECT current, max FROM character_resources WHERE character_id = ?1 AND name = ?2",
            params![p.character_id, p.name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional_ok()?;

    let (old_current, max) = row.ok_or_else(|| {
        anyhow::anyhow!(
            "resource {:?} not found on character {}",
            p.name,
            p.character_id
        )
    })?;

    let new_current = (old_current + p.delta).clamp(0, max);
    conn.execute(
        "UPDATE character_resources SET current = ?1 WHERE character_id = ?2 AND name = ?3",
        params![new_current, p.character_id, p.name],
    )
    .context("update character_resources.current")?;

    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "resource.adjusted",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Resource {name:?} adjusted on character id={char_id}: {old} → {new} (delta {sign}{delta})",
                name = p.name,
                char_id = p.character_id,
                old = old_current,
                new = new_current,
                sign = if p.delta >= 0 { "+" } else { "" },
                delta = p.delta,
            ),
            payload: serde_json::json!({
                "name": p.name,
                "old_current": old_current,
                "new_current": new_current,
                "delta_requested": p.delta,
                "max": max,
                "reason": p.reason,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    Ok(ResourceResult {
        character_id: p.character_id,
        name: p.name,
        current: new_current,
        max,
        event_id: emitted.event_id,
    })
}

/// Remove a resource row entirely. Emits `resource.removed`.
pub fn remove_resource(
    conn: &mut Connection,
    p: RemoveResourceParams,
) -> Result<ProficiencyResult> {
    conn.execute(
        "DELETE FROM character_resources WHERE character_id = ?1 AND name = ?2",
        params![p.character_id, p.name],
    )
    .context("delete character_resources")?;

    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "resource.removed",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Resource {name:?} removed from character id={char_id}",
                name = p.name,
                char_id = p.character_id,
            ),
            payload: serde_json::json!({ "name": p.name }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    // Reusing ProficiencyResult shape — same fields. Public API keeps them separate so
    // the type-level distinction holds if fields diverge later.
    Ok(ProficiencyResult {
        character_id: p.character_id,
        name: p.name,
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

    fn make_character(conn: &mut Connection) -> i64 {
        let p = CreateParams {
            name: "Kira".into(),
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
        };
        characters::create(conn, p).unwrap().character_id
    }

    #[test]
    fn set_proficiency_upserts() {
        let mut conn = fresh_conn();
        let c = make_character(&mut conn);
        set_proficiency(
            &mut conn,
            SetProficiencyParams {
                character_id: c,
                name: "stealth".into(),
                proficient: Some(true),
                expertise: None,
                ranks: None,
            },
        )
        .unwrap();
        // Second call with expertise — upsert.
        set_proficiency(
            &mut conn,
            SetProficiencyParams {
                character_id: c,
                name: "stealth".into(),
                proficient: Some(true),
                expertise: Some(true),
                ranks: None,
            },
        )
        .unwrap();
        let (prof, exp): (i64, i64) = conn
            .query_row(
                "SELECT proficient, expertise FROM character_proficiencies
                 WHERE character_id = ?1 AND name = 'stealth'",
                [c],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(prof, 1);
        assert_eq!(exp, 1);
    }

    #[test]
    fn resource_set_adjust_remove() {
        let mut conn = fresh_conn();
        let c = make_character(&mut conn);
        set_resource(
            &mut conn,
            SetResourceParams {
                character_id: c,
                name: "slot:1".into(),
                current: 4,
                max: 4,
                recharge: "long_rest".into(),
            },
        )
        .unwrap();

        // Adjust down by 1.
        let r = adjust_resource(
            &mut conn,
            AdjustResourceParams {
                character_id: c,
                name: "slot:1".into(),
                delta: -1,
                reason: Some("cast Magic Missile".into()),
            },
        )
        .unwrap();
        assert_eq!(r.current, 3);

        // Adjust way down — clamps to 0, not negative.
        let r = adjust_resource(
            &mut conn,
            AdjustResourceParams {
                character_id: c,
                name: "slot:1".into(),
                delta: -99,
                reason: None,
            },
        )
        .unwrap();
        assert_eq!(r.current, 0, "should clamp at 0");

        // Remove.
        remove_resource(
            &mut conn,
            RemoveResourceParams {
                character_id: c,
                name: "slot:1".into(),
            },
        )
        .unwrap();
        let present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM character_resources
                 WHERE character_id = ?1 AND name = 'slot:1'",
                [c],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(present, 0);
    }

    #[test]
    fn set_resource_rejects_invalid_recharge() {
        let mut conn = fresh_conn();
        let c = make_character(&mut conn);
        let err = set_resource(
            &mut conn,
            SetResourceParams {
                character_id: c,
                name: "slot:1".into(),
                current: 1,
                max: 1,
                recharge: "whenever".into(),
            },
        )
        .expect_err("should reject unknown recharge");
        assert!(format!("{err:#}").contains("recharge"));
    }

    #[test]
    fn adjust_nonexistent_resource_errors() {
        let mut conn = fresh_conn();
        let c = make_character(&mut conn);
        let err = adjust_resource(
            &mut conn,
            AdjustResourceParams {
                character_id: c,
                name: "slot:9".into(),
                delta: 1,
                reason: None,
            },
        )
        .expect_err("adjusting missing resource should fail");
        assert!(format!("{err:#}").contains("not found"));
    }
}
