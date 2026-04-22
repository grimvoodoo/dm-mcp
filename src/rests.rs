//! Short and long rest tools.
//!
//! Per `docs/characters.md §Resources`:
//!
//! - `rest.short` — refill resources whose `recharge = 'short_rest'`.
//! - `rest.long`  — refill resources whose `recharge ∈ {short_rest, long_rest, dawn}` AND
//!   restore `hp_current` to `hp_max`. Also resets death-save counters if the character is
//!   alive (a rest doesn't apply to a dead or unconscious character's save tally).

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::events::{self, EventSpec, Participant};

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ShortRestParams {
    pub character_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct LongRestParams {
    pub character_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestResult {
    pub character_id: i64,
    pub refilled_resources: Vec<RefilledResource>,
    pub hp_restored: Option<i32>,
    pub status_after: String,
    pub event_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefilledResource {
    pub name: String,
    pub from: i32,
    pub to: i32,
}

pub fn short_rest(conn: &mut Connection, p: ShortRestParams) -> Result<RestResult> {
    do_rest(conn, p.character_id, &["short_rest"], false, "rest.short")
}

pub fn long_rest(conn: &mut Connection, p: LongRestParams) -> Result<RestResult> {
    do_rest(
        conn,
        p.character_id,
        &["short_rest", "long_rest", "dawn"],
        true,
        "rest.long",
    )
}

fn do_rest(
    conn: &mut Connection,
    character_id: i64,
    recharge_kinds: &[&str],
    restore_hp: bool,
    event_kind: &str,
) -> Result<RestResult> {
    let (hp_current, hp_max, status): (i32, i32, String) = conn
        .query_row(
            "SELECT hp_current, hp_max, status FROM characters WHERE id = ?1",
            [character_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .with_context(|| format!("character {character_id} not found"))?;
    if status == "dead" {
        bail!("character {character_id} is dead and cannot rest");
    }

    // Read matching resources first so we can build the refill list.
    let placeholders: String = (0..recharge_kinds.len())
        .map(|i| format!("?{}", i + 2))
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT name, current, max FROM character_resources
         WHERE character_id = ?1 AND recharge IN ({placeholders})"
    );
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    params_vec.push(Box::new(character_id));
    for k in recharge_kinds {
        params_vec.push(Box::new(k.to_string()));
    }
    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn.prepare(&query)?;
    let to_refill: Vec<(String, i32, i32)> = stmt
        .query_map(params_refs.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let now = crate::world::current_campaign_hour(conn)?;
    let tx = conn.transaction().context("begin rest tx")?;

    let mut refilled = Vec::with_capacity(to_refill.len());
    for (name, current, max) in &to_refill {
        if *current >= *max {
            continue;
        }
        tx.execute(
            "UPDATE character_resources SET current = max
             WHERE character_id = ?1 AND name = ?2",
            params![character_id, name],
        )
        .context("refill resource")?;
        refilled.push(RefilledResource {
            name: name.clone(),
            from: *current,
            to: *max,
        });
    }

    let hp_restored = if restore_hp && hp_current < hp_max {
        tx.execute(
            "UPDATE characters
             SET hp_current = hp_max,
                 death_save_successes = 0, death_save_failures = 0,
                 updated_at = ?1
             WHERE id = ?2",
            params![now, character_id],
        )
        .context("restore hp")?;
        Some(hp_max - hp_current)
    } else if restore_hp {
        // Still clear death-save counters on long rest even if at full HP already.
        tx.execute(
            "UPDATE characters
             SET death_save_successes = 0, death_save_failures = 0,
                 updated_at = ?1
             WHERE id = ?2",
            params![now, character_id],
        )
        .context("reset death save counters")?;
        None
    } else {
        None
    };

    let new_status = status.clone();
    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: event_kind,
            campaign_hour: now,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={character_id} took a {kind} — {n} resource(s) refilled{hp}",
                kind = if restore_hp {
                    "long rest"
                } else {
                    "short rest"
                },
                n = refilled.len(),
                hp = match hp_restored {
                    Some(gained) => format!(", +{gained} HP"),
                    None => String::new(),
                }
            ),
            payload: serde_json::json!({
                "refilled": refilled,
                "hp_restored": hp_restored,
                "restore_hp": restore_hp,
            }),
            participants: &[Participant {
                character_id,
                role: "actor",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit rest tx")?;

    Ok(RestResult {
        character_id,
        refilled_resources: refilled,
        hp_restored,
        status_after: new_status,
        event_id: emitted.event_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::characters::{self, CreateParams as CharCreateParams};
    use crate::db::schema;
    use crate::proficiencies::{self, SetResourceParams};

    fn fresh() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        conn
    }

    fn mk(conn: &mut Connection) -> i64 {
        characters::create(
            conn,
            CharCreateParams {
                name: "K".into(),
                role: "player".into(),
                str_score: 10,
                dex_score: 10,
                con_score: 10,
                int_score: 10,
                wis_score: 10,
                cha_score: 10,
                hp_max: Some(20),
                hp_current: Some(5),
                armor_class: Some(12),
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
    fn short_rest_refills_short_rest_resources_only() {
        let mut conn = fresh();
        let id = mk(&mut conn);
        proficiencies::set_resource(
            &mut conn,
            SetResourceParams {
                character_id: id,
                name: "hit_die".into(),
                current: 0,
                max: 3,
                recharge: "short_rest".into(),
            },
        )
        .unwrap();
        proficiencies::set_resource(
            &mut conn,
            SetResourceParams {
                character_id: id,
                name: "slot:1".into(),
                current: 0,
                max: 4,
                recharge: "long_rest".into(),
            },
        )
        .unwrap();
        let r = short_rest(&mut conn, ShortRestParams { character_id: id }).unwrap();
        assert_eq!(r.refilled_resources.len(), 1);
        assert_eq!(r.refilled_resources[0].name, "hit_die");
        assert!(r.hp_restored.is_none());

        // Long-rest resource untouched.
        let slot: i32 = conn
            .query_row(
                "SELECT current FROM character_resources
                 WHERE character_id = ?1 AND name = 'slot:1'",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(slot, 0);
    }

    #[test]
    fn long_rest_refills_both_and_restores_hp() {
        let mut conn = fresh();
        let id = mk(&mut conn);
        proficiencies::set_resource(
            &mut conn,
            SetResourceParams {
                character_id: id,
                name: "hit_die".into(),
                current: 0,
                max: 3,
                recharge: "short_rest".into(),
            },
        )
        .unwrap();
        proficiencies::set_resource(
            &mut conn,
            SetResourceParams {
                character_id: id,
                name: "slot:1".into(),
                current: 0,
                max: 4,
                recharge: "long_rest".into(),
            },
        )
        .unwrap();
        let r = long_rest(&mut conn, LongRestParams { character_id: id }).unwrap();
        assert_eq!(r.refilled_resources.len(), 2);
        assert_eq!(r.hp_restored, Some(15));
        let hp: i32 = conn
            .query_row(
                "SELECT hp_current FROM characters WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hp, 20);
    }
}
