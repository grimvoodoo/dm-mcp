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
use rusqlite::{params, Connection, Transaction};
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
    let question = content
        .setup_questions
        .iter()
        .find(|q| q.id == p.question_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown question_id {:?}; valid: {:?}",
                p.question_id,
                content
                    .setup_questions
                    .iter()
                    .map(|q| q.id.as_str())
                    .collect::<Vec<_>>()
            )
        })?;

    validate_answer_against_question(question, &p.answer)?;

    let answer_json = serde_json::to_string(&p.answer).context("encode answer")?;
    let now = wall_clock_millis();

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

/// Validate an answer JSON value against a question's `multi`/`options`/`or_free_text`
/// declaration:
/// - multi=true: answer must be an array of strings; each must be in `options` (unless
///   `or_free_text` allows free text)
/// - multi=false: answer must be a string; in `options` (unless `or_free_text`)
fn validate_answer_against_question(q: &SetupQuestion, answer: &serde_json::Value) -> Result<()> {
    if q.multi {
        let arr = answer.as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "question {:?} is multi-select; answer must be a JSON array of strings, got {:?}",
                q.id,
                answer
            )
        })?;
        for v in arr {
            let s = v.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "question {:?} multi-select array elements must be strings; got {:?}",
                    q.id,
                    v
                )
            })?;
            ensure_option_or_free_text(q, s)?;
        }
    } else {
        let s = answer.as_str().ok_or_else(|| {
            anyhow::anyhow!(
                "question {:?} is single-choice; answer must be a JSON string, got {:?}",
                q.id,
                answer
            )
        })?;
        ensure_option_or_free_text(q, s)?;
    }
    Ok(())
}

fn ensure_option_or_free_text(q: &SetupQuestion, s: &str) -> Result<()> {
    if q.options.iter().any(|opt| opt == s) {
        return Ok(());
    }
    if q.or_free_text {
        return Ok(());
    }
    bail!(
        "answer {:?} is not in question {:?}'s allowed options (and free text is disabled); valid: {:?}",
        s,
        q.id,
        q.options
    );
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

    // Refuse re-runs: a world.generated event already in the log means a starting zone is
    // in place. Re-running would silently double the world's geography.
    if world_already_generated(conn)? {
        bail!(
            "generate_world has already run for this campaign (a world.generated event \
             exists). Re-running would create a second starting zone and a duplicate set \
             of neighbours."
        );
    }

    let starting_zone_name = format!("Starting {biome}", biome = starting_biome.replace('_', " "));

    // Pick how many stub neighbours: 2–5, biased toward the lower end. Use the dice RNG
    // so this respects any test seed if we add seeding later. RNG decisions made up-front
    // (outside the transaction) so a transaction retry replays deterministically.
    let mut rng = rand::rng();
    let neighbour_count: u8 = rng.random_range(2..=5);
    const DIRECTIONS: &[&str] = &["n", "ne", "e", "se", "s", "sw", "w", "nw"];
    let mut chosen: Vec<&'static str> = DIRECTIONS.to_vec();
    for i in (1..chosen.len()).rev() {
        let j = rng.random_range(0..=i);
        chosen.swap(i, j);
    }
    let chosen_dirs: Vec<&'static str> =
        chosen.into_iter().take(neighbour_count as usize).collect();

    // Single transaction wraps zone inserts + connections + the world.generated event.
    // Either every row is committed together, or none — no half-built world.
    let tx = conn.transaction().context("begin generate_world tx")?;

    let starting_zone_id = insert_zone_tx(
        &tx,
        &starting_zone_name,
        &starting_biome,
        "wilderness",
        "small",
        None,
    )
    .context("insert starting zone")?;

    let mut neighbour_ids = Vec::with_capacity(chosen_dirs.len());
    for (i, dir) in chosen_dirs.iter().enumerate() {
        let neighbour_name = format!("Unexplored {label}", label = direction_label(dir));
        let neighbour_id = insert_zone_tx(
            &tx,
            &neighbour_name,
            // Neighbours inherit the starting biome as a stub default — Phase 7's
            // full-generation pass will refine.
            &starting_biome,
            "wilderness",
            "small",
            None,
        )
        .with_context(|| format!("insert neighbour {i}"))?;
        insert_connection_tx(
            &tx,
            starting_zone_id,
            neighbour_id,
            travel_time_for(dir),
            "wilderness",
            dir,
        )?;
        insert_connection_tx(
            &tx,
            neighbour_id,
            starting_zone_id,
            travel_time_for(dir),
            "wilderness",
            opposite_direction(dir),
        )?;
        neighbour_ids.push(neighbour_id);
    }

    let emitted = events::emit_in_tx(
        &tx,
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

    tx.commit().context("commit generate_world tx")?;

    Ok(GenerateWorldResult {
        starting_zone_id,
        starting_zone_name,
        starting_biome,
        neighbour_zone_ids: neighbour_ids,
        event_id: emitted.event_id,
    })
}

fn world_already_generated(conn: &Connection) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'world.generated'",
            [],
            |row| row.get(0),
        )
        .context("check for prior world.generated event")?;
    Ok(count > 0)
}

// ── mark_ready ────────────────────────────────────────────────────────────────

pub fn mark_ready(conn: &mut Connection, p: MarkReadyParams) -> Result<MarkReadyResult> {
    let phase = read_phase(conn)?;
    if phase != "setup" {
        bail!("mark_ready called twice — campaign is already in phase {phase:?}");
    }
    if !world_already_generated(conn)? {
        bail!(
            "mark_ready requires generate_world to have run first — without it the running \
             campaign would have no starting zone, and a later generate_world call would be \
             refused (it only runs in setup phase)."
        );
    }
    let started_at = wall_clock_millis();

    // If the caller passes a player character, look up where they're standing so we can
    // mark their starting zone as visited. The player obviously knows where their own
    // character is — pre-seeding the knowledge here means the very first world.map call
    // sees the starting zone without an extra setup step.
    let player_starting_zone: Option<i64> = match p.player_character_id {
        None => None,
        Some(pcid) => {
            // current_zone_id is itself an Option<i64> in the row, so we get
            // Result<Option<i64>> from query_row.
            match conn.query_row(
                "SELECT current_zone_id FROM characters WHERE id = ?1",
                [pcid],
                |row| row.get::<_, Option<i64>>(0),
            ) {
                Ok(zid) => zid,
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(e).context("read player character's current_zone_id"),
            }
        }
    };

    let participants: Vec<crate::events::Participant<'_>> = match p.player_character_id {
        Some(pcid) => vec![crate::events::Participant {
            character_id: pcid,
            role: "actor",
        }],
        None => vec![],
    };

    // Single transaction wraps the campaign_state flip + the campaign.started event so
    // an interrupted call cannot leave the singleton mid-transition or the event missing.
    let tx = conn.transaction().context("begin mark_ready tx")?;

    tx.execute(
        "UPDATE campaign_state
         SET phase = 'running', started_at = ?1, player_character_id = ?2
         WHERE id = 1",
        params![started_at, p.player_character_id],
    )
    .context("flip campaign_state to running")?;

    // Pre-seed the player's knowledge of their starting zone.
    if let (Some(pcid), Some(zid)) = (p.player_character_id, player_starting_zone) {
        tx.execute(
            "INSERT INTO character_zone_knowledge (character_id, zone_id, level, last_visit_at_hour)
             VALUES (?1, ?2, 'visited', 0)
             ON CONFLICT(character_id, zone_id) DO UPDATE SET level = 'visited'",
            params![pcid, zid],
        )
        .context("seed player knowledge of starting zone")?;
    }

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "campaign.started",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!("Campaign started at {started_at} (epoch ms)"),
            payload: serde_json::json!({
                "started_at": started_at,
                "player_character_id": p.player_character_id,
            }),
            participants: &participants,
            items: &[],
        },
    )?;

    tx.commit().context("commit mark_ready tx")?;

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

/// Transaction-aware variant — used by generate_world inside its single tx.
fn insert_zone_tx(
    tx: &Transaction<'_>,
    name: &str,
    biome: &str,
    kind: &str,
    size: &str,
    parent_zone_id: Option<i64>,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO zones (name, biome, kind, size, parent_zone_id, encounter_tags)
         VALUES (?1, ?2, ?3, ?4, ?5, '[]')",
        params![name, biome, kind, size, parent_zone_id],
    )
    .context("insert zones row")?;
    Ok(tx.last_insert_rowid())
}

fn insert_connection_tx(
    tx: &Transaction<'_>,
    from: i64,
    to: i64,
    hours: i32,
    mode: &str,
    direction: &str,
) -> Result<()> {
    tx.execute(
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

fn wall_clock_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
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

    #[test]
    fn mark_ready_requires_generate_world_first() {
        let (mut conn, _content) = fresh();
        // Phase is 'setup', no world.generated event yet.
        let err = mark_ready(
            &mut conn,
            MarkReadyParams {
                player_character_id: None,
            },
        )
        .expect_err("mark_ready without generate_world should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("generate_world"),
            "error should explain the precondition: {msg}"
        );
    }

    #[test]
    fn generate_world_refuses_double_invocation() {
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
        let err =
            generate_world(&mut conn, &content).expect_err("second generate_world should fail");
        assert!(format!("{err:#}").contains("already run"));
    }

    #[test]
    fn answer_rejects_value_outside_question_options() {
        let (mut conn, content) = fresh();
        // tone is single-choice grim/balanced/heroic and disables free text.
        let err = answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "tone".into(),
                answer: serde_json::json!("ridiculous"),
            },
        )
        .expect_err("invalid tone should be rejected");
        assert!(format!("{err:#}").contains("not in question"));
    }

    #[test]
    fn answer_rejects_array_for_single_choice_question() {
        let (mut conn, content) = fresh();
        let err = answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "tone".into(),
                answer: serde_json::json!(["grim", "heroic"]),
            },
        )
        .expect_err("array answer to single-choice should fail");
        assert!(format!("{err:#}").contains("single-choice"));
    }

    #[test]
    fn answer_rejects_string_for_multi_choice_question() {
        let (mut conn, content) = fresh();
        let err = answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "enemy_preference".into(),
                answer: serde_json::json!("undead"),
            },
        )
        .expect_err("string answer to multi-select should fail");
        assert!(format!("{err:#}").contains("multi-select"));
    }

    #[test]
    fn answer_accepts_free_text_when_allowed() {
        let (mut conn, content) = fresh();
        // starting_biome has or_free_text=true, so a non-listed value is OK.
        answer(
            &mut conn,
            &content,
            AnswerParams {
                question_id: "starting_biome".into(),
                answer: serde_json::json!("ash_wastes_of_the_old_war"),
            },
        )
        .expect("free-text biome should be accepted");
    }
}
