//! Campaign bootstrap phase.
//!
//! Phase 6 surface (per Roadmap):
//!
//! - [`new_campaign`] — confirms the campaign is in `setup` phase and returns the list of
//!   setup questions from `content/campaign/setup_questions.yaml`.
//! - [`answer`] — upserts a question_id → answer (JSON-encoded) into
//!   `campaign_setup_answers`.
//! - [`generate_world`] — uses the recorded `starting_biome` answer to create a starting
//!   zone + 2–5 stub neighbours, with directed connections back to it.
//! - [`mark_ready`] — flips `campaign_state.phase` to `running`, records the wall-clock
//!   moment in `started_at`, emits a `campaign.started` event.
//!
//! The `campaign_hour` clock stays at 0 throughout — Phase 6 is the moment the clock
//! starts. Subsequent phases (Phase 7 travel, Phase 9 combat) advance it.

use anyhow::{bail, Context, Result};
use rand::RngExt;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::content::{Content, SetupQuestion};
use crate::events::{self, EventSpec};

// ── Tool params / results ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NewCampaignParams {}

#[derive(Debug, Clone, Serialize)]
pub struct NewCampaignResult {
    pub phase: String,
    pub questions: Vec<SetupQuestion>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AnswerParams {
    pub question_id: String,
    /// JSON-encoded answer. May be a string, an array of strings (multi-select), or any
    /// other JSON value.
    pub answer: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnswerResult {
    pub question_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GenerateWorldParams {}

#[derive(Debug, Clone, Serialize)]
pub struct GenerateWorldResult {
    pub starting_zone_id: i64,
    pub starting_zone_name: String,
    pub starting_biome: String,
    pub neighbour_zone_ids: Vec<i64>,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MarkReadyParams {
    /// Optional player character — recorded as `campaign_state.player_character_id` so
    /// downstream tools can derive "who is the player?" without a separate query.
    #[serde(default)]
    pub player_character_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MarkReadyResult {
    pub phase: String,
    pub started_at: i64,
    pub event_id: i64,
}

// ── new_campaign ──────────────────────────────────────────────────────────────

pub fn new_campaign(conn: &Connection, content: &Content) -> Result<NewCampaignResult> {
    let phase = read_phase(conn)?;
    if phase != "setup" {
        bail!(
            "campaign is in phase {phase:?}, not 'setup'; new_campaign should only run before mark_ready"
        );
    }
    Ok(NewCampaignResult {
        phase,
        questions: content.setup_questions.clone(),
    })
}

// ── answer ────────────────────────────────────────────────────────────────────

pub fn answer(conn: &mut Connection, content: &Content, p: AnswerParams) -> Result<AnswerResult> {
    let phase = read_phase(conn)?;
    if phase != "setup" {
        bail!("cannot record an answer once the campaign has left 'setup' (currently {phase:?})");
    }
    if !content
        .setup_questions
        .iter()
        .any(|q| q.id == p.question_id)
    {
        bail!(
            "unknown question_id {:?}; valid: {:?}",
            p.question_id,
            content
                .setup_questions
                .iter()
                .map(|q| q.id.as_str())
                .collect::<Vec<_>>()
        );
    }

    let answer_json = serde_json::to_string(&p.answer).context("encode answer")?;
    // Wall-clock for the audit trail; campaign_hour is still 0.
    let now = wall_clock_seconds();

    conn.execute(
        "INSERT INTO campaign_setup_answers (question_id, answer, answered_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(question_id) DO UPDATE SET
            answer = excluded.answer,
            answered_at = excluded.answered_at",
        params![p.question_id, answer_json, now],
    )
    .context("upsert campaign_setup_answers")?;

    Ok(AnswerResult {
        question_id: p.question_id,
    })
}

// ── generate_world ────────────────────────────────────────────────────────────

pub fn generate_world(conn: &mut Connection, _content: &Content) -> Result<GenerateWorldResult> {
    let phase = read_phase(conn)?;
    if phase != "setup" {
        bail!("generate_world should run during the setup phase (current phase {phase:?})");
    }

    // Required answer.
    let starting_biome = read_string_answer(conn, "starting_biome")?
        .ok_or_else(|| anyhow::anyhow!("starting_biome answer not recorded"))?;

    // Match the biome against content if possible — Phase 6 only ships one biome
    // (temperate_forest) but a free-text answer is also accepted by the Roadmap. If the
    // biome is unknown we still create the zone using the literal biome string.
    let starting_zone_name = format!("Starting {biome}", biome = starting_biome.replace('_', " "));

    // Insert the starting zone.
    let starting_zone_id = insert_zone(
        conn,
        &starting_zone_name,
        &starting_biome,
        "wilderness",
        "small",
        None,
    )
    .context("insert starting zone")?;

    // Pick how many stub neighbours: 2–5, biased toward the lower end. Use the dice RNG
    // so this respects any test seed if we add seeding later.
    let mut rng = rand::rng();
    let neighbour_count: u8 = rng.random_range(2..=5);

    // Compass directions for Phase 6 — we lay neighbours around the starting zone in
    // distinct directions so the eventual map renderer doesn't pile them on top of each
    // other.
    const DIRECTIONS: &[&str] = &["n", "ne", "e", "se", "s", "sw", "w", "nw"];
    let mut chosen: Vec<&'static str> = DIRECTIONS.to_vec();
    // Trivial Fisher–Yates so the chosen directions vary across runs.
    for i in (1..chosen.len()).rev() {
        let j = rng.random_range(0..=i);
        chosen.swap(i, j);
    }

    let mut neighbour_ids = Vec::with_capacity(neighbour_count as usize);
    for (i, dir) in chosen.iter().take(neighbour_count as usize).enumerate() {
        let neighbour_name = format!("Unexplored {label}", label = direction_label(dir));
        let neighbour_id = insert_zone(
            conn,
            &neighbour_name,
            // Neighbours inherit the starting biome as a stub default — Phase 7's
            // full-generation pass will refine.
            &starting_biome,
            "wilderness",
            "small",
            None,
        )
        .with_context(|| format!("insert neighbour {i}"))?;
        // Forward edge.
        insert_connection(
            conn,
            starting_zone_id,
            neighbour_id,
            travel_time_for(dir),
            "wilderness",
            dir,
        )?;
        // Reverse edge — direction flipped.
        insert_connection(
            conn,
            neighbour_id,
            starting_zone_id,
            travel_time_for(dir),
            "wilderness",
            opposite_direction(dir),
        )?;
        neighbour_ids.push(neighbour_id);
    }

    // Emit a single "world.generated" event capturing the zones created.
    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "world.generated",
            campaign_hour: 0,
            combat_round: None,
            zone_id: Some(starting_zone_id),
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Generated starting zone {starting_zone_name:?} (biome {starting_biome:?}) with {n} neighbour stub(s)",
                n = neighbour_ids.len(),
            ),
            payload: serde_json::json!({
                "starting_zone_id": starting_zone_id,
                "starting_zone_name": starting_zone_name,
                "starting_biome": starting_biome,
                "neighbour_zone_ids": neighbour_ids,
            }),
            participants: &[],
            items: &[],
        },
    )?;

    Ok(GenerateWorldResult {
        starting_zone_id,
        starting_zone_name,
        starting_biome,
        neighbour_zone_ids: neighbour_ids,
        event_id: emitted.event_id,
    })
}

// ── mark_ready ────────────────────────────────────────────────────────────────

pub fn mark_ready(conn: &mut Connection, p: MarkReadyParams) -> Result<MarkReadyResult> {
    let phase = read_phase(conn)?;
    if phase != "setup" {
        bail!("mark_ready called twice — campaign is already in phase {phase:?}");
    }
    let started_at = wall_clock_seconds();

    conn.execute(
        "UPDATE campaign_state
         SET phase = 'running', started_at = ?1, player_character_id = ?2
         WHERE id = 1",
        params![started_at, p.player_character_id],
    )
    .context("flip campaign_state to running")?;

    let mut participants = Vec::new();
    if let Some(pcid) = p.player_character_id {
        participants.push(crate::events::Participant {
            character_id: pcid,
            role: "actor",
        });
    }
    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "campaign.started",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!("Campaign started at {started_at} (epoch seconds)"),
            payload: serde_json::json!({
                "started_at": started_at,
                "player_character_id": p.player_character_id,
            }),
            participants: &participants,
            items: &[],
        },
    )?;

    Ok(MarkReadyResult {
        phase: "running".to_string(),
        started_at,
        event_id: emitted.event_id,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn read_phase(conn: &Connection) -> Result<String> {
    conn.query_row("SELECT phase FROM campaign_state WHERE id = 1", [], |row| {
        row.get(0)
    })
    .context("read campaign_state.phase")
}

fn read_string_answer(conn: &Connection, question_id: &str) -> Result<Option<String>> {
    let row: Option<String> = match conn.query_row(
        "SELECT answer FROM campaign_setup_answers WHERE question_id = ?1",
        [question_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(s) => Some(s),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e).context("read campaign_setup_answers"),
    };
    let Some(raw) = row else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).context("decode campaign_setup_answers.answer JSON")?;
    Ok(value.as_str().map(|s| s.to_string()))
}

fn insert_zone(
    conn: &Connection,
    name: &str,
    biome: &str,
    kind: &str,
    size: &str,
    parent_zone_id: Option<i64>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO zones (name, biome, kind, size, parent_zone_id, encounter_tags)
         VALUES (?1, ?2, ?3, ?4, ?5, '[]')",
        params![name, biome, kind, size, parent_zone_id],
    )
    .context("insert zones row")?;
    Ok(conn.last_insert_rowid())
}

fn insert_connection(
    conn: &Connection,
    from: i64,
    to: i64,
    hours: i32,
    mode: &str,
    direction: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO zone_connections
            (from_zone_id, to_zone_id, travel_time_hours, travel_mode, one_way, direction_from)
         VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        params![from, to, hours, mode, direction],
    )
    .context("insert zone_connections row")?;
    Ok(())
}

fn travel_time_for(_direction: &str) -> i32 {
    // Phase 6 picks a small uniform cost so neighbour reachability tests run quickly.
    // Phase 7's travel tool will respect this value when advancing campaign_hour.
    2
}

fn opposite_direction(d: &str) -> &'static str {
    match d {
        "n" => "s",
        "s" => "n",
        "e" => "w",
        "w" => "e",
        "ne" => "sw",
        "sw" => "ne",
        "nw" => "se",
        "se" => "nw",
        "up" => "down",
        "down" => "up",
        _ => "n",
    }
}

fn direction_label(d: &str) -> &'static str {
    match d {
        "n" => "North",
        "ne" => "Northeast",
        "e" => "East",
        "se" => "Southeast",
        "s" => "South",
        "sw" => "Southwest",
        "w" => "West",
        "nw" => "Northwest",
        "up" => "Above",
        "down" => "Below",
        _ => "Beyond",
    }
}

fn wall_clock_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;

    fn fresh() -> (Connection, Content) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        (conn, Content::load(None).unwrap())
    }

    #[test]
    fn new_campaign_returns_questions_in_setup_phase() {
        let (conn, content) = fresh();
        let r = new_campaign(&conn, &content).unwrap();
        assert_eq!(r.phase, "setup");
        assert!(r.questions.iter().any(|q| q.id == "starting_biome"));
        assert!(r.questions.iter().any(|q| q.id == "tone"));
    }

    #[test]
    fn answer_upserts() {
        let (mut conn, content) = fresh();
        answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "starting_biome".into(),
                answer: serde_json::json!("temperate_forest"),
            },
        )
        .unwrap();
        // Re-answer.
        answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "starting_biome".into(),
                answer: serde_json::json!("plains"),
            },
        )
        .unwrap();
        let stored: String = conn
            .query_row(
                "SELECT answer FROM campaign_setup_answers WHERE question_id = 'starting_biome'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Stored as JSON-encoded — the value will be a quoted string.
        assert!(stored.contains("plains"));
    }

    #[test]
    fn answer_rejects_unknown_question() {
        let (mut conn, content) = fresh();
        let err = answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "made_up_question".into(),
                answer: serde_json::json!("anything"),
            },
        )
        .expect_err("should reject unknown question");
        assert!(format!("{err:#}").contains("unknown question_id"));
    }

    #[test]
    fn generate_world_creates_zone_plus_neighbours() {
        let (mut conn, content) = fresh();
        answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "starting_biome".into(),
                answer: serde_json::json!("temperate_forest"),
            },
        )
        .unwrap();
        let r = generate_world(&mut conn, &content).expect("generate_world");
        assert!(r.starting_zone_id > 0);
        assert!(r.starting_biome == "temperate_forest");
        assert!(
            (2..=5).contains(&r.neighbour_zone_ids.len()),
            "expected 2-5 neighbours, got {}",
            r.neighbour_zone_ids.len()
        );

        // Bidirectional connections exist.
        for n in &r.neighbour_zone_ids {
            let c: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM zone_connections
                     WHERE (from_zone_id = ?1 AND to_zone_id = ?2)
                        OR (from_zone_id = ?2 AND to_zone_id = ?1)",
                    [r.starting_zone_id, *n],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                c, 2,
                "expected forward+reverse edges between starting and neighbour {n}"
            );
        }
    }

    #[test]
    fn generate_world_requires_starting_biome_answer() {
        let (mut conn, content) = fresh();
        let err =
            generate_world(&mut conn, &content).expect_err("missing starting_biome should fail");
        assert!(format!("{err:#}").contains("starting_biome"));
    }

    #[test]
    fn mark_ready_flips_phase_and_emits_event() {
        let (mut conn, content) = fresh();
        // Provide answer + generate world so the flow is realistic.
        answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "starting_biome".into(),
                answer: serde_json::json!("temperate_forest"),
            },
        )
        .unwrap();
        generate_world(&mut conn, &content).unwrap();

        let r = mark_ready(
            &mut conn,
            MarkReadyParams {
                player_character_id: None,
            },
        )
        .expect("mark_ready");

        assert_eq!(r.phase, "running");

        let phase: String = conn
            .query_row("SELECT phase FROM campaign_state WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(phase, "running");

        let kind: String = conn
            .query_row(
                "SELECT kind FROM events WHERE id = ?1",
                [r.event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kind, "campaign.started");
    }

    #[test]
    fn mark_ready_refuses_double_invocation() {
        let (mut conn, content) = fresh();
        answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "starting_biome".into(),
                answer: serde_json::json!("temperate_forest"),
            },
        )
        .unwrap();
        generate_world(&mut conn, &content).unwrap();
        mark_ready(
            &mut conn,
            MarkReadyParams {
                player_character_id: None,
            },
        )
        .unwrap();
        let err = mark_ready(
            &mut conn,
            MarkReadyParams {
                player_character_id: None,
            },
        )
        .expect_err("second mark_ready should fail");
        assert!(format!("{err:#}").contains("already in phase"));
    }
}
