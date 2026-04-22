//! Effects: apply a temporary numerical modifier to a character's stats; dispel it later.
//!
//! See `docs/checks.md §Effects` for the model. Phase 4 scope:
//!
//! - [`apply`] inserts a row into `effects` and emits `effect.applied`.
//! - [`dispel`] flips `active = 0` and emits `effect.expired(reason="dispelled")`.
//!
//! Effects are read by [`crate::characters::get`] which composes effective ability /
//! AC / speed from base + sum of modifiers per `target_key`.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::events::{self, EventSpec, Participant};

const VALID_TARGET_KINDS: &[&str] = &[
    "ability", "ac", "speed", "hp_max", "attack", "damage", "skill", "save", "misc",
];

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ApplyParams {
    pub target_character_id: i64,
    /// Free-text origin, e.g. `"potion:giant-strength"`, `"curse:rotspeak"`, `"spell:bless"`.
    pub source: String,
    /// Coarse category. One of: `ability`, `ac`, `speed`, `hp_max`, `attack`, `damage`,
    /// `skill`, `save`, `misc`.
    pub target_kind: String,
    /// Specific key, e.g. `str_score`, `armor_class`, `stealth`, `save:con`.
    pub target_key: String,
    pub modifier: i32,
    /// Optional dice expression rolled per-check (e.g. Bless adds `1d4` on attacks/saves).
    /// Phase 4 stores the string; `resolve_check` (Phase 5) will roll it.
    #[serde(default)]
    pub dice_expr: Option<String>,
    #[serde(default)]
    pub expires_at_hour: Option<i64>,
    #[serde(default)]
    pub expires_after_rounds: Option<i32>,
    #[serde(default)]
    pub expires_on_dispel: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyResult {
    pub effect_id: i64,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DispelParams {
    pub effect_id: i64,
    /// Free-text narrative reason — recorded in the event payload.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DispelResult {
    pub effect_id: i64,
    pub event_id: i64,
}

/// Record an effect and emit `effect.applied`.
pub fn apply(conn: &mut Connection, p: ApplyParams) -> Result<ApplyResult> {
    if !VALID_TARGET_KINDS.contains(&p.target_kind.as_str()) {
        bail!(
            "unknown target_kind {:?}; valid: {VALID_TARGET_KINDS:?}",
            p.target_kind
        );
    }

    // Emit the event first so we can store its id on the effects row as start_event_id.
    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "effect.applied",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Applied {source} to character id={target} ({target_kind}:{target_key} {sign}{modifier})",
                source = p.source,
                target = p.target_character_id,
                target_kind = p.target_kind,
                target_key = p.target_key,
                sign = if p.modifier >= 0 { "+" } else { "" },
                modifier = p.modifier,
            ),
            payload: serde_json::json!({
                "source": p.source,
                "target_kind": p.target_kind,
                "target_key": p.target_key,
                "modifier": p.modifier,
                "dice_expr": p.dice_expr,
                "expires_at_hour": p.expires_at_hour,
                "expires_after_rounds": p.expires_after_rounds,
                "expires_on_dispel": p.expires_on_dispel.unwrap_or(false),
            }),
            participants: &[Participant {
                character_id: p.target_character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    conn.execute(
        "INSERT INTO effects (
            target_character_id, source, target_kind, target_key,
            modifier, dice_expr,
            start_event_id, expires_at_hour, expires_after_rounds,
            expires_on_dispel, active
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
        params![
            p.target_character_id,
            p.source,
            p.target_kind,
            p.target_key,
            p.modifier,
            p.dice_expr,
            emitted.event_id,
            p.expires_at_hour,
            p.expires_after_rounds,
            i64::from(p.expires_on_dispel.unwrap_or(false)),
        ],
    )
    .context("insert effects row")?;

    Ok(ApplyResult {
        effect_id: conn.last_insert_rowid(),
        event_id: emitted.event_id,
    })
}

/// Deactivate an effect and emit `effect.expired(reason='dispelled')`.
///
/// Returns an error if the effect is already inactive — callers should not double-dispel
/// (which would suggest a bug in tool orchestration).
pub fn dispel(conn: &mut Connection, p: DispelParams) -> Result<DispelResult> {
    // Read the row so we can include its details in the event payload.
    let row: Option<(i64, String, String, String, i32, i64)> = conn
        .query_row(
            "SELECT target_character_id, source, target_kind, target_key, modifier, active
             FROM effects WHERE id = ?1",
            [p.effect_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional_ok()?;

    let (target_character_id, source, target_kind, target_key, modifier, active) =
        row.ok_or_else(|| anyhow::anyhow!("effect {} not found", p.effect_id))?;

    if active == 0 {
        bail!("effect {} is already inactive", p.effect_id);
    }

    // Emit first so event_id can be recorded if we ever want to track what dispelled what;
    // the effects row itself only keeps start_event_id.
    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "effect.expired",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Dispelled {source} on character id={target_character_id} ({target_kind}:{target_key})",
                source = source,
                target_character_id = target_character_id,
                target_kind = target_kind,
                target_key = target_key,
            ),
            payload: serde_json::json!({
                "effect_id": p.effect_id,
                "source": source,
                "target_kind": target_kind,
                "target_key": target_key,
                "modifier": modifier,
                "reason": p.reason.clone().unwrap_or_else(|| "dispelled".to_string()),
                "expiry_reason": "dispelled",
            }),
            participants: &[Participant {
                character_id: target_character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    conn.execute("UPDATE effects SET active = 0 WHERE id = ?1", [p.effect_id])
        .context("deactivate effects row")?;

    Ok(DispelResult {
        effect_id: p.effect_id,
        event_id: emitted.event_id,
    })
}

/// Rusqlite helper — turn "no rows" into `None`.
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
            str_score: 14,
            dex_score: 10,
            con_score: 10,
            int_score: 10,
            wis_score: 10,
            cha_score: 10,
            hp_max: Some(10),
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
    fn apply_and_dispel_round_trip() {
        let mut conn = fresh_conn();
        let char_id = make_character(&mut conn);

        // Base STR is 14. Apply +4, get shows 18.
        let applied = apply(
            &mut conn,
            ApplyParams {
                target_character_id: char_id,
                source: "potion:bulls-strength".into(),
                target_kind: "ability".into(),
                target_key: "str_score".into(),
                modifier: 4,
                dice_expr: None,
                expires_at_hour: None,
                expires_after_rounds: None,
                expires_on_dispel: Some(true),
            },
        )
        .expect("apply");
        let view = characters::get(&conn, char_id).unwrap();
        assert_eq!(view.effective_str, 18, "14 base + 4 effect");

        // Dispel — get shows 14 again.
        let dispelled = dispel(
            &mut conn,
            DispelParams {
                effect_id: applied.effect_id,
                reason: Some("potion wore off".into()),
            },
        )
        .expect("dispel");
        let view = characters::get(&conn, char_id).unwrap();
        assert_eq!(view.effective_str, 14, "base restored after dispel");

        // Event log has effect.applied + effect.expired referencing the right character.
        let kinds: Vec<String> = conn
            .prepare(
                "SELECT e.kind FROM events e
                 JOIN event_participants ep ON ep.event_id = e.id
                 WHERE ep.character_id = ?1
                 ORDER BY e.id",
            )
            .unwrap()
            .query_map([char_id], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(kinds.contains(&"character.created".into()));
        assert!(kinds.contains(&"effect.applied".into()));
        assert!(kinds.contains(&"effect.expired".into()));
        assert_eq!(dispelled.effect_id, applied.effect_id);
    }

    #[test]
    fn apply_rejects_unknown_target_kind() {
        let mut conn = fresh_conn();
        let char_id = make_character(&mut conn);
        let err = apply(
            &mut conn,
            ApplyParams {
                target_character_id: char_id,
                source: "garbage".into(),
                target_kind: "wiggly".into(), // not in VALID_TARGET_KINDS
                target_key: "str_score".into(),
                modifier: 1,
                dice_expr: None,
                expires_at_hour: None,
                expires_after_rounds: None,
                expires_on_dispel: None,
            },
        )
        .expect_err("should reject unknown target_kind");
        assert!(format!("{err:#}").contains("target_kind"));
    }

    #[test]
    fn dispel_rejects_already_inactive() {
        let mut conn = fresh_conn();
        let char_id = make_character(&mut conn);
        let applied = apply(
            &mut conn,
            ApplyParams {
                target_character_id: char_id,
                source: "potion".into(),
                target_kind: "ability".into(),
                target_key: "str_score".into(),
                modifier: 1,
                dice_expr: None,
                expires_at_hour: None,
                expires_after_rounds: None,
                expires_on_dispel: None,
            },
        )
        .unwrap();
        dispel(
            &mut conn,
            DispelParams {
                effect_id: applied.effect_id,
                reason: None,
            },
        )
        .unwrap();

        let err = dispel(
            &mut conn,
            DispelParams {
                effect_id: applied.effect_id,
                reason: None,
            },
        )
        .expect_err("double-dispel should fail");
        assert!(format!("{err:#}").contains("already inactive"));
    }
}
