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
use rusqlite::{params, Connection, Transaction};
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

/// Insert an event plus its junction rows inside an existing transaction. Use this when
/// the calling tool needs to bundle DB writes plus the event into a single atomic unit
/// (e.g. setup::generate_world creating zones + emitting world.generated). The caller owns
/// the transaction and must commit it.
/// Hard cap on the `summary` text written to the events table. Several callers format
/// user-influenced strings (condition names, effect sources, character names) into the
/// summary via `format!`. A maliciously- or accidentally-long input would otherwise
/// land verbatim in every event row, bloating the WAL and slowing recall queries.
/// 1 KiB is comfortably above the longest legitimate summary in the codebase
/// (`condition.applied`, `combat.apply_damage` etc. are all < 200 chars).
const MAX_SUMMARY_BYTES: usize = 1024;

pub fn emit_in_tx(tx: &Transaction<'_>, spec: &EventSpec<'_>) -> Result<EmittedEvent> {
    let payload = serde_json::to_string(&spec.payload).context("serialize payload")?;
    let summary = truncate_summary(&spec.summary);

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
            summary,
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

    Ok(EmittedEvent { event_id })
}

/// Insert an event plus its junction rows inside its own transaction. The standalone
/// version used when the caller doesn't need atomicity beyond the event itself.
pub fn emit(conn: &mut Connection, spec: &EventSpec<'_>) -> Result<EmittedEvent> {
    let tx = conn.transaction().context("begin event tx")?;
    let result = emit_in_tx(&tx, spec)?;
    tx.commit().context("commit event tx")?;
    Ok(result)
}

/// Cap `summary` to `MAX_SUMMARY_BYTES`, truncating on a UTF-8 char boundary so the
/// resulting string is still valid (`String::truncate` panics on a non-boundary, and
/// SQLite would happily store half a multi-byte sequence). If truncated, append a
/// trailing `…` so a reader sees the cut-off rather than thinking the string ends mid-
/// word. Operates on `&str` and returns owned only when truncation is needed.
fn truncate_summary(s: &str) -> std::borrow::Cow<'_, str> {
    if s.len() <= MAX_SUMMARY_BYTES {
        return std::borrow::Cow::Borrowed(s);
    }
    // Find the last char boundary at or before MAX_SUMMARY_BYTES - 3 (room for "…",
    // which is 3 UTF-8 bytes). is_char_boundary works on byte indices.
    let target = MAX_SUMMARY_BYTES.saturating_sub(3);
    let mut cut = target;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(MAX_SUMMARY_BYTES);
    out.push_str(&s[..cut]);
    out.push('…');
    std::borrow::Cow::Owned(out)
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

    #[test]
    fn long_summary_is_truncated_with_ellipsis() {
        // A pathological summary (e.g. someone passing a 1MB condition source name)
        // shouldn't land verbatim in events.summary. The cap defends WAL size +
        // recall query latency.
        let mut conn = fresh_conn();
        let kira = insert_character(&conn, "Kira");
        let huge = "x".repeat(MAX_SUMMARY_BYTES + 200);
        let spec = EventSpec {
            kind: "test.long",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: huge.clone(),
            payload: serde_json::json!({}),
            participants: &[Participant {
                character_id: kira,
                role: "actor",
            }],
            items: &[],
        };
        let emitted = emit(&mut conn, &spec).expect("emit");

        let stored: String = conn
            .query_row(
                "SELECT summary FROM events WHERE id = ?1",
                [emitted.event_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            stored.len() <= MAX_SUMMARY_BYTES,
            "stored summary {} bytes should fit under the cap {}",
            stored.len(),
            MAX_SUMMARY_BYTES
        );
        assert!(
            stored.ends_with('…'),
            "truncated summary should end with the ellipsis marker; got tail {:?}",
            &stored[stored.len().saturating_sub(8)..]
        );
    }

    #[test]
    fn short_summary_passes_through_unchanged() {
        let mut conn = fresh_conn();
        let kira = insert_character(&conn, "Kira");
        let short = "Short summary that should not be touched.";
        let spec = EventSpec {
            kind: "test.short",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: short.to_string(),
            payload: serde_json::json!({}),
            participants: &[Participant {
                character_id: kira,
                role: "actor",
            }],
            items: &[],
        };
        let emitted = emit(&mut conn, &spec).expect("emit");
        let stored: String = conn
            .query_row(
                "SELECT summary FROM events WHERE id = ?1",
                [emitted.event_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, short);
    }
}
