//! Event-log writer.
//!
//! Every mutating tool emits at least one event via [`emit`]. An event is a row in
//! `events` plus zero-or-more rows in `event_participants` / `event_items`, all written in
//! a single transaction so the insert is atomic — readers can't see a half-finished event.
//!
//! See `docs/history-log.md` for the schema-level rationale.
//!
//! Phase 4 uses `campaign_hour = 0` for all events (the campaign clock doesn't advance
//! until Phase 6's `setup.mark_ready`). Later phases will thread a real clock through.
//! Backstory synthesis in Phase 8 will emit events with negative `campaign_hour`.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;

/// One row in `event_participants`. The role string comes from the
/// `('actor','target','witness','beneficiary')` CHECK set — callers pass the string; the
/// DB enforces validity.
#[derive(Debug, Clone, Copy)]
pub struct Participant<'a> {
    pub character_id: i64,
    pub role: &'a str,
}

/// One row in `event_items`.
#[derive(Debug, Clone, Copy)]
pub struct ItemRef<'a> {
    pub item_id: i64,
    pub role: &'a str,
}

/// Full description of an event to write. The only mandatory fields are `kind`, `summary`,
/// `payload`, and `campaign_hour`. Everything else is optional and nullable.
pub struct EventSpec<'a> {
    pub kind: &'a str,
    pub campaign_hour: i64,
    pub combat_round: Option<i32>,
    pub zone_id: Option<i64>,
    pub encounter_id: Option<i64>,
    pub parent_id: Option<i64>,
    pub summary: String,
    pub payload: Value,
    pub participants: &'a [Participant<'a>],
    pub items: &'a [ItemRef<'a>],
}

/// Record returned after a successful emit. The event id can be threaded into follow-up
/// events via their `parent_id`, or into rows like `effects.start_event_id`.
#[derive(Debug, Clone, Serialize)]
pub struct EmittedEvent {
    pub event_id: i64,
}

/// Insert an event plus its junction rows inside one transaction. Returns the new event id.
pub fn emit(conn: &mut Connection, spec: &EventSpec<'_>) -> Result<EmittedEvent> {
    let tx = conn.transaction().context("begin event tx")?;

    let payload = serde_json::to_string(&spec.payload).context("serialize payload")?;

    tx.execute(
        "INSERT INTO events
           (kind, campaign_hour, combat_round, zone_id, encounter_id, parent_id, summary, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            spec.kind,
            spec.campaign_hour,
            spec.combat_round,
            spec.zone_id,
            spec.encounter_id,
            spec.parent_id,
            spec.summary,
            payload,
        ],
    )
    .context("insert events row")?;
    let event_id = tx.last_insert_rowid();

    for p in spec.participants {
        tx.execute(
            "INSERT INTO event_participants (event_id, character_id, role) VALUES (?1, ?2, ?3)",
            params![event_id, p.character_id, p.role],
        )
        .with_context(|| {
            format!(
                "insert event_participants row (event={event_id}, char={}, role={:?})",
                p.character_id, p.role
            )
        })?;
    }

    for it in spec.items {
        tx.execute(
            "INSERT INTO event_items (event_id, item_id, role) VALUES (?1, ?2, ?3)",
            params![event_id, it.item_id, it.role],
        )
        .with_context(|| {
            format!(
                "insert event_items row (event={event_id}, item={}, role={:?})",
                it.item_id, it.role
            )
        })?;
    }

    tx.commit().context("commit event tx")?;
    Ok(EmittedEvent { event_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;

    fn fresh_conn() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        conn
    }

    fn insert_character(conn: &Connection, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO characters (
                name, role,
                str_score, dex_score, con_score, int_score, wis_score, cha_score,
                hp_current, hp_max, armor_class,
                created_at, updated_at
            ) VALUES (?1, 'player', 10,10,10,10,10,10, 10, 10, 10, 0, 0)",
            [name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn emit_writes_row_plus_junctions() {
        let mut conn = fresh_conn();
        let kira = insert_character(&conn, "Kira");

        let spec = EventSpec {
            kind: "test.hello",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: "Kira says hello".to_string(),
            payload: serde_json::json!({ "msg": "hi" }),
            participants: &[Participant {
                character_id: kira,
                role: "actor",
            }],
            items: &[],
        };
        let emitted = emit(&mut conn, &spec).expect("emit");
        assert!(emitted.event_id > 0);

        let (kind, summary, payload): (String, String, String) = conn
            .query_row(
                "SELECT kind, summary, payload FROM events WHERE id = ?1",
                [emitted.event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "test.hello");
        assert_eq!(summary, "Kira says hello");
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["msg"], "hi");

        let (char_id, role): (i64, String) = conn
            .query_row(
                "SELECT character_id, role FROM event_participants WHERE event_id = ?1",
                [emitted.event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(char_id, kira);
        assert_eq!(role, "actor");
    }

    #[test]
    fn emit_rolls_back_on_participant_fk_failure() {
        let mut conn = fresh_conn();
        let spec = EventSpec {
            kind: "test.bad",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: "nope".to_string(),
            payload: serde_json::json!({}),
            // Character 99_999 doesn't exist — FK violation on the junction insert.
            participants: &[Participant {
                character_id: 99_999,
                role: "actor",
            }],
            items: &[],
        };
        let err = emit(&mut conn, &spec).expect_err("bad FK should fail");
        assert!(
            format!("{err:#}").contains("FOREIGN KEY") || format!("{err:#}").contains("foreign")
        );

        // Transaction rolled back — no orphan events row should exist.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "events row should have been rolled back");
    }

    #[test]
    fn emit_supports_multiple_participants() {
        let mut conn = fresh_conn();
        let a = insert_character(&conn, "A");
        let b = insert_character(&conn, "B");
        let spec = EventSpec {
            kind: "social.bargain",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: "A offers a thing to B".to_string(),
            payload: serde_json::json!({}),
            participants: &[
                Participant {
                    character_id: a,
                    role: "actor",
                },
                Participant {
                    character_id: b,
                    role: "target",
                },
            ],
            items: &[],
        };
        let emitted = emit(&mut conn, &spec).expect("emit");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_participants WHERE event_id = ?1",
                [emitted.event_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }
}
