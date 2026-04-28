//! Characters: CRUD + effective-stat composition.
//!
//! The Phase 4 surface:
//!
//! - [`create`] inserts a row into `characters` and emits a `character.created` event.
//! - [`get`] reads a row, composes effective ability scores by summing active modifier
//!   effects on top of the base stats, loads proficiencies / conditions / resources /
//!   active effects, and hands back a fat JSON-serialisable struct.
//! - [`update_plans`] / [`change_role`] are small updates with matching events.
//!
//! See `docs/characters.md` for the full entity model.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::events::{self, EventSpec, Participant};

// ── Input params ──────────────────────────────────────────────────────────────

/// Arguments for [`create`]. Required fields are `name`, `role`, and the six ability
/// scores. Everything else has a documented default.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CreateParams {
    pub name: String,
    /// One of: `player`, `companion`, `friendly`, `enemy`, `neutral`.
    pub role: String,

    // Base ability scores — the six d20-inspired stats.
    pub str_score: i32,
    pub dex_score: i32,
    pub con_score: i32,
    pub int_score: i32,
    pub wis_score: i32,
    pub cha_score: i32,

    /// Defaults to 10 if not supplied. Phase 4 doesn't compute HP from class/level yet.
    #[serde(default)]
    pub hp_max: Option<i32>,
    /// Defaults to `hp_max`.
    #[serde(default)]
    pub hp_current: Option<i32>,
    #[serde(default)]
    pub armor_class: Option<i32>,
    #[serde(default)]
    pub speed_ft: Option<i32>,
    #[serde(default)]
    pub initiative_bonus: Option<i32>,
    /// One of: `tiny`, `small`, `medium`, `large`, `huge`, `gargantuan`. Default `medium`.
    #[serde(default)]
    pub size: Option<String>,

    #[serde(default)]
    pub species: Option<String>,
    #[serde(default)]
    pub class_or_archetype: Option<String>,
    #[serde(default)]
    pub ideology: Option<String>,
    #[serde(default)]
    pub backstory: Option<String>,
    #[serde(default)]
    pub plans: Option<String>,

    /// 0–100; default 50.
    #[serde(default)]
    pub loyalty: Option<i32>,

    #[serde(default)]
    pub party_id: Option<i64>,
    #[serde(default)]
    pub current_zone_id: Option<i64>,
}

/// Result of [`create`].
#[derive(Debug, Clone, Serialize)]
pub struct CreateResult {
    pub character_id: i64,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetParams {
    pub character_id: i64,
}

/// Arguments for [`update_plans`].
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UpdatePlansParams {
    pub character_id: i64,
    /// New prose for the character's current agenda/motivations. Pass an empty string to
    /// clear.
    pub new_plans: String,
    /// Optional — what caused the plan to change. Recorded in the event payload.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ChangeRoleParams {
    pub character_id: i64,
    /// New role — one of `player`, `companion`, `friendly`, `enemy`, `neutral`.
    pub new_role: String,
    /// Free-text reason for the pivot (recorded in the event payload).
    pub reason: String,
}

/// Update-result carrying the event id of the emitted event.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateResult {
    pub character_id: i64,
    pub event_id: i64,
}

// ── Output shape for `character.get` ──────────────────────────────────────────

/// Flattened, client-friendly character readout. Base stats come straight from the row;
/// `effective_*` fields layer on the sum of active effect modifiers, so a DM-agent can
/// read effective values directly without doing their own math.
#[derive(Debug, Clone, Serialize)]
pub struct CharacterView {
    pub id: i64,
    pub name: String,
    pub role: String,
    pub party_id: Option<i64>,

    // Base ability scores.
    pub str_score: i32,
    pub dex_score: i32,
    pub con_score: i32,
    pub int_score: i32,
    pub wis_score: i32,
    pub cha_score: i32,

    // Effective = base + sum of active modifier effects with target_key=`<stat>_score`.
    pub effective_str: i32,
    pub effective_dex: i32,
    pub effective_con: i32,
    pub effective_int: i32,
    pub effective_wis: i32,
    pub effective_cha: i32,

    pub hp_current: i32,
    pub hp_max: i32,
    pub hp_temp: i32,
    pub armor_class: i32,
    pub effective_armor_class: i32,
    pub speed_ft: i32,
    pub effective_speed_ft: i32,
    pub initiative_bonus: i32,

    pub level: i32,
    pub xp_total: i32,
    pub proficiency_bonus: i32,

    pub size: String,
    pub species: Option<String>,
    pub class_or_archetype: Option<String>,
    pub ideology: Option<String>,
    pub backstory: Option<String>,
    pub plans: Option<String>,

    pub loyalty: i32,
    pub status: String,
    pub current_zone_id: Option<i64>,
    pub death_save_successes: i32,
    pub death_save_failures: i32,

    pub proficiencies: Vec<ProficiencyRow>,
    pub resources: Vec<ResourceRow>,
    pub active_effects: Vec<EffectRow>,
    pub active_conditions: Vec<ConditionRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProficiencyRow {
    pub name: String,
    pub proficient: bool,
    pub expertise: bool,
    pub ranks: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceRow {
    pub name: String,
    pub current: i32,
    pub max: i32,
    pub recharge: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectRow {
    pub id: i64,
    pub source: String,
    pub target_kind: String,
    pub target_key: String,
    pub modifier: i32,
    pub dice_expr: Option<String>,
    pub expires_at_hour: Option<i64>,
    pub expires_after_rounds: Option<i32>,
    pub expires_on_dispel: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConditionRow {
    pub id: i64,
    pub condition: String,
    pub severity: i32,
    /// Event id of whatever caused this condition, if the application call chained one.
    pub source_event_id: Option<i64>,
    /// Optional in-game-hour expiry (decremented at hour-time, not round-time).
    pub expires_at_hour: Option<i64>,
    /// Optional round-based expiry. Ticked by `combat.next_turn` at round boundaries.
    pub expires_after_rounds: Option<i32>,
    /// Optional save-on-retry spec (e.g. `save:con:dc15`). The DM agent reads this to
    /// know whether the character can attempt a periodic save to throw the condition off.
    pub remove_on_save: Option<String>,
}

// ── Implementation ────────────────────────────────────────────────────────────

const VALID_ROLES: &[&str] = &["player", "companion", "friendly", "enemy", "neutral"];
const VALID_SIZES: &[&str] = &["tiny", "small", "medium", "large", "huge", "gargantuan"];

fn default_hp() -> i32 {
    10
}
fn default_ac() -> i32 {
    10
}
fn default_speed() -> i32 {
    30
}
fn default_size() -> String {
    "medium".to_string()
}
fn default_loyalty() -> i32 {
    50
}

/// Insert a character and emit `character.created`. Returns the new id + the event id.
pub fn create(conn: &mut Connection, p: CreateParams) -> Result<CreateResult> {
    if !VALID_ROLES.contains(&p.role.as_str()) {
        bail!("unknown role {:?}; valid: {VALID_ROLES:?}", p.role);
    }
    let size = p.size.clone().unwrap_or_else(default_size);
    if !VALID_SIZES.contains(&size.as_str()) {
        bail!("unknown size {:?}; valid: {VALID_SIZES:?}", size);
    }

    // Defensive bounds (#32). The MCP boundary is the trust line — an LLM agent SHOULD
    // pass sane values, but the tool surface needs to defend against the cases where
    // it doesn't (or where a future agent does something unexpected). Without these
    // checks, e.g. str_score=999 produces a giant ability_modifier that breaks every
    // downstream check, and i32::MAX HP overflows when added to. Bounds picked
    // generously to accommodate exotic content (giants, dragons) without being
    // unbounded.
    for (label, value) in &[
        ("str_score", p.str_score),
        ("dex_score", p.dex_score),
        ("con_score", p.con_score),
        ("int_score", p.int_score),
        ("wis_score", p.wis_score),
        ("cha_score", p.cha_score),
    ] {
        if !(1..=30).contains(value) {
            bail!("{label} must be in 1..=30 (got {value})");
        }
    }

    let hp_max = p.hp_max.unwrap_or_else(default_hp);
    if !(1..=10_000).contains(&hp_max) {
        bail!("hp_max must be in 1..=10_000 (got {hp_max})");
    }
    let hp_current = p.hp_current.unwrap_or(hp_max);
    if !(0..=hp_max).contains(&hp_current) {
        bail!("hp_current must be in 0..=hp_max ({hp_max}) (got {hp_current})");
    }
    let armor_class = p.armor_class.unwrap_or_else(default_ac);
    if !(1..=50).contains(&armor_class) {
        bail!("armor_class must be in 1..=50 (got {armor_class})");
    }
    let speed_ft = p.speed_ft.unwrap_or_else(default_speed);
    if !(0..=1_000).contains(&speed_ft) {
        bail!("speed_ft must be in 0..=1_000 (got {speed_ft})");
    }
    let initiative_bonus = p.initiative_bonus.unwrap_or(0);
    if !(-20..=20).contains(&initiative_bonus) {
        bail!("initiative_bonus must be in -20..=20 (got {initiative_bonus})");
    }
    let loyalty = p.loyalty.unwrap_or_else(default_loyalty);
    if !(0..=100).contains(&loyalty) {
        // The schema's CHECK constraint enforces this too, but bailing here gives a
        // clearer error than the FK violation that would surface at INSERT time.
        bail!("loyalty must be in 0..=100 (got {loyalty})");
    }

    // Single transaction wraps the row insert, the optional zone-knowledge seed, AND
    // the character.created event so all three commit atomically. Pre-fix, the event
    // emitted on a separate top-level call meant a process crash between the two
    // could leave a character row with no matching event in the log (issue #29).
    // tx.last_insert_rowid() resolves the FK reference inside the same tx, so the
    // event participant row points at the new id without needing a commit first.
    let tx = conn.transaction().context("begin create tx")?;
    tx.execute(
        "INSERT INTO characters (
            name, role, party_id,
            str_score, dex_score, con_score, int_score, wis_score, cha_score,
            hp_current, hp_max, hp_temp,
            armor_class, speed_ft, initiative_bonus,
            size,
            species, class_or_archetype, ideology, backstory, plans,
            loyalty,
            current_zone_id,
            created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3,
            ?4, ?5, ?6, ?7, ?8, ?9,
            ?10, ?11, 0,
            ?12, ?13, ?14,
            ?15,
            ?16, ?17, ?18, ?19, ?20,
            ?21,
            ?22,
            0, 0
        )",
        params![
            p.name,
            p.role,
            p.party_id,
            p.str_score,
            p.dex_score,
            p.con_score,
            p.int_score,
            p.wis_score,
            p.cha_score,
            hp_current,
            hp_max,
            armor_class,
            speed_ft,
            initiative_bonus,
            size,
            p.species,
            p.class_or_archetype,
            p.ideology,
            p.backstory,
            p.plans,
            loyalty,
            p.current_zone_id,
        ],
    )
    .context("insert characters row")?;
    let character_id = tx.last_insert_rowid();

    // Seed knowledge of the starting zone. A character placed at a zone "knows"
    // they are there — without this, world.describe_zone refuses to describe the
    // character's own current location and world.map can't anchor at it.
    // Order-independent with setup.mark_ready: both upsert visited, idempotently.
    if let Some(zone_id) = p.current_zone_id {
        tx.execute(
            "INSERT INTO character_zone_knowledge
                (character_id, zone_id, level, last_visit_at_hour)
             VALUES (?1, ?2, 'visited', 0)
             ON CONFLICT(character_id, zone_id) DO UPDATE SET level = 'visited'",
            params![character_id, zone_id],
        )
        .context("seed character knowledge of starting zone")?;
    }

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "character.created",
            campaign_hour: 0,
            combat_round: None,
            zone_id: p.current_zone_id,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Created {role} {name:?} (id={character_id})",
                role = p.role,
                name = p.name,
            ),
            payload: serde_json::json!({
                "role": p.role,
                "name": p.name,
                "str_score": p.str_score,
                "dex_score": p.dex_score,
                "con_score": p.con_score,
                "int_score": p.int_score,
                "wis_score": p.wis_score,
                "cha_score": p.cha_score,
                "hp_max": hp_max,
                "armor_class": armor_class,
            }),
            participants: &[Participant {
                character_id,
                role: "actor",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit create tx")?;

    Ok(CreateResult {
        character_id,
        event_id: emitted.event_id,
    })
}

/// Read a character + compose effective stats.
pub fn get(conn: &Connection, character_id: i64) -> Result<CharacterView> {
    let base = read_base_row(conn, character_id)?;

    // Active effects for this character — sum modifiers by target_key so we can layer
    // them on top of base stats.
    let effects = load_active_effects(conn, character_id)?;

    let sum_for = |key: &str| -> i32 {
        effects
            .iter()
            .filter(|e| e.target_key == key)
            .map(|e| e.modifier)
            .sum()
    };

    let effective_str = base.str_score + sum_for("str_score");
    let effective_dex = base.dex_score + sum_for("dex_score");
    let effective_con = base.con_score + sum_for("con_score");
    let effective_int = base.int_score + sum_for("int_score");
    let effective_wis = base.wis_score + sum_for("wis_score");
    let effective_cha = base.cha_score + sum_for("cha_score");
    let effective_armor_class = base.armor_class + sum_for("armor_class");
    let effective_speed_ft = base.speed_ft + sum_for("speed_ft");

    let proficiencies = load_proficiencies(conn, character_id)?;
    let resources = load_resources(conn, character_id)?;
    let active_conditions = load_active_conditions(conn, character_id)?;

    Ok(CharacterView {
        id: base.id,
        name: base.name,
        role: base.role,
        party_id: base.party_id,
        str_score: base.str_score,
        dex_score: base.dex_score,
        con_score: base.con_score,
        int_score: base.int_score,
        wis_score: base.wis_score,
        cha_score: base.cha_score,
        effective_str,
        effective_dex,
        effective_con,
        effective_int,
        effective_wis,
        effective_cha,
        hp_current: base.hp_current,
        hp_max: base.hp_max,
        hp_temp: base.hp_temp,
        armor_class: base.armor_class,
        effective_armor_class,
        speed_ft: base.speed_ft,
        effective_speed_ft,
        initiative_bonus: base.initiative_bonus,
        level: base.level,
        xp_total: base.xp_total,
        proficiency_bonus: base.proficiency_bonus,
        size: base.size,
        species: base.species,
        class_or_archetype: base.class_or_archetype,
        ideology: base.ideology,
        backstory: base.backstory,
        plans: base.plans,
        loyalty: base.loyalty,
        status: base.status,
        current_zone_id: base.current_zone_id,
        death_save_successes: base.death_save_successes,
        death_save_failures: base.death_save_failures,
        proficiencies,
        resources,
        active_effects: effects,
        active_conditions,
    })
}

/// Update the `plans` prose field and emit `npc.plan_changed`.
pub fn update_plans(conn: &mut Connection, p: UpdatePlansParams) -> Result<UpdateResult> {
    // Single tx so the row UPDATE + the npc.plan_changed event commit atomically.
    // Pre-fix the event was emitted on a separate top-level call; a process crash
    // between the two left the plans field mutated with no event in the log
    // (issue #29 — append-only event-log invariant).
    let tx = conn.transaction().context("begin update_plans tx")?;
    let old_plans: Option<String> = tx
        .query_row(
            "SELECT plans FROM characters WHERE id = ?1",
            [p.character_id],
            |row| row.get(0),
        )
        .optional_ok()?
        .ok_or_else(|| anyhow::anyhow!("character {} not found", p.character_id))?;

    tx.execute(
        "UPDATE characters SET plans = ?1, updated_at = 0 WHERE id = ?2",
        params![p.new_plans, p.character_id],
    )
    .context("update plans")?;

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "npc.plan_changed",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!("Plans updated for character id={}", p.character_id),
            payload: serde_json::json!({
                "old_plans": old_plans,
                "new_plans": p.new_plans,
                "reason": p.reason,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit update_plans tx")?;

    Ok(UpdateResult {
        character_id: p.character_id,
        event_id: emitted.event_id,
    })
}

/// Change a character's `role` and emit `npc.role_changed`.
pub fn change_role(conn: &mut Connection, p: ChangeRoleParams) -> Result<UpdateResult> {
    if !VALID_ROLES.contains(&p.new_role.as_str()) {
        bail!("unknown role {:?}; valid: {VALID_ROLES:?}", p.new_role);
    }

    // Same atomicity rationale as update_plans (#29).
    let tx = conn.transaction().context("begin change_role tx")?;
    let old_role: String = tx
        .query_row(
            "SELECT role FROM characters WHERE id = ?1",
            [p.character_id],
            |row| row.get(0),
        )
        .optional_ok()?
        .ok_or_else(|| anyhow::anyhow!("character {} not found", p.character_id))?;

    tx.execute(
        "UPDATE characters SET role = ?1, updated_at = 0 WHERE id = ?2",
        params![p.new_role, p.character_id],
    )
    .context("update role")?;

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "npc.role_changed",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Role changed from {old_role:?} to {:?} for character id={}",
                p.new_role, p.character_id
            ),
            payload: serde_json::json!({
                "old_role": old_role,
                "new_role": p.new_role,
                "reason": p.reason,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "target",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit change_role tx")?;

    Ok(UpdateResult {
        character_id: p.character_id,
        event_id: emitted.event_id,
    })
}

// ── Internal row readers ──────────────────────────────────────────────────────

struct BaseRow {
    id: i64,
    name: String,
    role: String,
    party_id: Option<i64>,
    str_score: i32,
    dex_score: i32,
    con_score: i32,
    int_score: i32,
    wis_score: i32,
    cha_score: i32,
    hp_current: i32,
    hp_max: i32,
    hp_temp: i32,
    armor_class: i32,
    speed_ft: i32,
    initiative_bonus: i32,
    level: i32,
    xp_total: i32,
    proficiency_bonus: i32,
    size: String,
    species: Option<String>,
    class_or_archetype: Option<String>,
    ideology: Option<String>,
    backstory: Option<String>,
    plans: Option<String>,
    loyalty: i32,
    status: String,
    current_zone_id: Option<i64>,
    death_save_successes: i32,
    death_save_failures: i32,
}

fn read_base_row(conn: &Connection, id: i64) -> Result<BaseRow> {
    conn.query_row(
        "SELECT
            id, name, role, party_id,
            str_score, dex_score, con_score, int_score, wis_score, cha_score,
            hp_current, hp_max, hp_temp,
            armor_class, speed_ft, initiative_bonus,
            level, xp_total, proficiency_bonus,
            size,
            species, class_or_archetype, ideology, backstory, plans,
            loyalty, status, current_zone_id,
            death_save_successes, death_save_failures
         FROM characters WHERE id = ?1",
        [id],
        |row| {
            Ok(BaseRow {
                id: row.get(0)?,
                name: row.get(1)?,
                role: row.get(2)?,
                party_id: row.get(3)?,
                str_score: row.get(4)?,
                dex_score: row.get(5)?,
                con_score: row.get(6)?,
                int_score: row.get(7)?,
                wis_score: row.get(8)?,
                cha_score: row.get(9)?,
                hp_current: row.get(10)?,
                hp_max: row.get(11)?,
                hp_temp: row.get(12)?,
                armor_class: row.get(13)?,
                speed_ft: row.get(14)?,
                initiative_bonus: row.get(15)?,
                level: row.get(16)?,
                xp_total: row.get(17)?,
                proficiency_bonus: row.get(18)?,
                size: row.get(19)?,
                species: row.get(20)?,
                class_or_archetype: row.get(21)?,
                ideology: row.get(22)?,
                backstory: row.get(23)?,
                plans: row.get(24)?,
                loyalty: row.get(25)?,
                status: row.get(26)?,
                current_zone_id: row.get(27)?,
                death_save_successes: row.get(28)?,
                death_save_failures: row.get(29)?,
            })
        },
    )
    .with_context(|| format!("character {id} not found"))
}

fn load_active_effects(conn: &Connection, character_id: i64) -> Result<Vec<EffectRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, source, target_kind, target_key, modifier, dice_expr,
                expires_at_hour, expires_after_rounds, expires_on_dispel
         FROM effects
         WHERE target_character_id = ?1 AND active = 1
         ORDER BY id",
    )?;
    let rows: Vec<EffectRow> = stmt
        .query_map([character_id], |row| {
            Ok(EffectRow {
                id: row.get(0)?,
                source: row.get(1)?,
                target_kind: row.get(2)?,
                target_key: row.get(3)?,
                modifier: row.get(4)?,
                dice_expr: row.get(5)?,
                expires_at_hour: row.get(6)?,
                expires_after_rounds: row.get(7)?,
                expires_on_dispel: row.get::<_, i64>(8)? != 0,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn load_proficiencies(conn: &Connection, character_id: i64) -> Result<Vec<ProficiencyRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, proficient, expertise, ranks
         FROM character_proficiencies
         WHERE character_id = ?1
         ORDER BY name",
    )?;
    let rows: Vec<ProficiencyRow> = stmt
        .query_map([character_id], |row| {
            Ok(ProficiencyRow {
                name: row.get(0)?,
                proficient: row.get::<_, i64>(1)? != 0,
                expertise: row.get::<_, i64>(2)? != 0,
                ranks: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn load_resources(conn: &Connection, character_id: i64) -> Result<Vec<ResourceRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, current, max, recharge
         FROM character_resources
         WHERE character_id = ?1
         ORDER BY name",
    )?;
    let rows: Vec<ResourceRow> = stmt
        .query_map([character_id], |row| {
            Ok(ResourceRow {
                name: row.get(0)?,
                current: row.get(1)?,
                max: row.get(2)?,
                recharge: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn load_active_conditions(conn: &Connection, character_id: i64) -> Result<Vec<ConditionRow>> {
    // Surface every optional field stored on character_conditions, mirroring how
    // load_active_effects exposes effects' full shape. Without this, fields like
    // remove_on_save are write-only — the DM agent can apply a condition with a save
    // spec but can never read it back to know the spec exists.
    let mut stmt = conn.prepare(
        "SELECT id, condition, severity,
                source_event_id, expires_at_hour, expires_after_rounds, remove_on_save
         FROM character_conditions
         WHERE character_id = ?1 AND active = 1
         ORDER BY id",
    )?;
    let rows: Vec<ConditionRow> = stmt
        .query_map([character_id], |row| {
            Ok(ConditionRow {
                id: row.get(0)?,
                condition: row.get(1)?,
                severity: row.get(2)?,
                source_event_id: row.get(3)?,
                expires_at_hour: row.get(4)?,
                expires_after_rounds: row.get(5)?,
                remove_on_save: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// ── Optional-row helper ───────────────────────────────────────────────────────
//
// `query_row` returns `QueryReturnedNoRows` for a missing row. Wrap it in `Option` so
// callers can pattern-match without an explicit `is_err` check against that specific
// variant.

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
    use crate::db::schema;

    fn fresh_conn() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        conn
    }

    fn sample_params(name: &str) -> CreateParams {
        CreateParams {
            name: name.to_string(),
            role: "player".to_string(),
            str_score: 14,
            dex_score: 12,
            con_score: 13,
            int_score: 10,
            wis_score: 11,
            cha_score: 9,
            hp_max: Some(12),
            hp_current: None,
            armor_class: Some(13),
            speed_ft: Some(30),
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
        }
    }

    #[test]
    fn create_inserts_row_and_emits_event() {
        let mut conn = fresh_conn();
        let r = create(&mut conn, sample_params("Kira")).expect("create");
        assert!(r.character_id > 0);
        assert!(r.event_id > 0);

        let name: String = conn
            .query_row(
                "SELECT name FROM characters WHERE id = ?1",
                [r.character_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Kira");

        // Event row present with correct kind + participant role.
        let (kind, char_id): (String, i64) = conn
            .query_row(
                "SELECT e.kind, ep.character_id
                 FROM events e JOIN event_participants ep ON ep.event_id = e.id
                 WHERE e.id = ?1",
                [r.event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "character.created");
        assert_eq!(char_id, r.character_id);
    }

    #[test]
    fn create_rejects_bad_role() {
        let mut conn = fresh_conn();
        let mut p = sample_params("X");
        p.role = "villain".into();
        assert!(create(&mut conn, p).is_err());
    }

    #[test]
    fn create_rejects_out_of_range_ability_scores() {
        // Each ability score; pick a few representative bad values. Coerce closures to
        // the same `fn(&mut CreateParams)` type — without that, each closure literal
        // has a unique anonymous type and the array doesn't unify.
        type Mutator = fn(&mut CreateParams);
        let mut conn = fresh_conn();
        let mutators: &[(&str, Mutator)] = &[
            ("str_score=999", |p| p.str_score = 999),
            ("dex_score=-5", |p| p.dex_score = -5),
            ("con_score=0", |p| p.con_score = 0),
            ("int_score=31", |p| p.int_score = 31),
        ];
        for (label, mutator) in mutators {
            let mut p = sample_params("X");
            mutator(&mut p);
            let err = create(&mut conn, p).expect_err(label);
            let msg = format!("{err:#}");
            assert!(
                msg.contains("1..=30"),
                "{label}: error should cite the bound: {msg}"
            );
        }
    }

    #[test]
    fn create_rejects_inverted_hp_relationship() {
        let mut conn = fresh_conn();
        let mut p = sample_params("X");
        p.hp_max = Some(10);
        p.hp_current = Some(50);
        let err = create(&mut conn, p).expect_err("hp_current > hp_max should bail");
        let msg = format!("{err:#}");
        assert!(msg.contains("hp_current") && msg.contains("hp_max"));
    }

    #[test]
    fn create_rejects_out_of_range_loyalty() {
        let mut conn = fresh_conn();
        let mut p = sample_params("X");
        p.loyalty = Some(150);
        let err = create(&mut conn, p).expect_err("loyalty > 100 should bail");
        assert!(format!("{err:#}").contains("loyalty"));
    }

    #[test]
    fn create_rejects_overflow_hp_max() {
        let mut conn = fresh_conn();
        let mut p = sample_params("X");
        p.hp_max = Some(i32::MAX);
        let err = create(&mut conn, p).expect_err("i32::MAX hp should bail");
        assert!(format!("{err:#}").contains("hp_max"));
    }

    #[test]
    fn get_returns_base_stats_matching_create() {
        let mut conn = fresh_conn();
        let r = create(&mut conn, sample_params("Kira")).unwrap();
        let v = get(&conn, r.character_id).expect("get");
        assert_eq!(v.str_score, 14);
        assert_eq!(v.effective_str, 14, "no effects yet, effective == base");
        assert_eq!(v.armor_class, 13);
        assert_eq!(v.effective_armor_class, 13);
    }

    #[test]
    fn get_composes_effective_stats_from_active_effects() {
        let mut conn = fresh_conn();
        let r = create(&mut conn, sample_params("Kira")).unwrap();
        // Insert an active +4 STR effect directly (effects module does this properly; this
        // is the unit-level check that `get` composes correctly).
        // First we need an event to reference as start_event_id — use the character.created
        // event.
        conn.execute(
            "INSERT INTO effects (
                target_character_id, source, target_kind, target_key,
                modifier, dice_expr,
                start_event_id, expires_at_hour, expires_after_rounds,
                expires_on_dispel, active
            ) VALUES (?1, 'potion:bulls-strength', 'ability', 'str_score',
                      4, NULL, ?2, NULL, NULL, 1, 1)",
            params![r.character_id, r.event_id],
        )
        .unwrap();

        let v = get(&conn, r.character_id).unwrap();
        assert_eq!(v.str_score, 14, "base unchanged");
        assert_eq!(v.effective_str, 18, "14 + 4 = 18");
        assert_eq!(v.active_effects.len(), 1);
    }

    #[test]
    fn update_plans_changes_row_and_emits_event() {
        let mut conn = fresh_conn();
        let r = create(&mut conn, sample_params("Kira")).unwrap();
        let u = update_plans(
            &mut conn,
            UpdatePlansParams {
                character_id: r.character_id,
                new_plans: "Find the missing blacksmith.".to_string(),
                reason: Some("Village hooks".to_string()),
            },
        )
        .expect("update_plans");
        assert!(u.event_id > 0);
        let plans: Option<String> = conn
            .query_row(
                "SELECT plans FROM characters WHERE id = ?1",
                [r.character_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(plans.as_deref(), Some("Find the missing blacksmith."));
        let kind: String = conn
            .query_row(
                "SELECT kind FROM events WHERE id = ?1",
                [u.event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kind, "npc.plan_changed");
    }

    #[test]
    fn change_role_flips_role_and_emits_event() {
        let mut conn = fresh_conn();
        let r = create(&mut conn, sample_params("Grog")).unwrap();
        let u = change_role(
            &mut conn,
            ChangeRoleParams {
                character_id: r.character_id,
                new_role: "enemy".to_string(),
                reason: "revealed as a spy".to_string(),
            },
        )
        .expect("change_role");
        let (role, kind): (String, String) = conn
            .query_row(
                "SELECT c.role, e.kind
                 FROM characters c, events e
                 WHERE c.id = ?1 AND e.id = ?2",
                [r.character_id, u.event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(role, "enemy");
        assert_eq!(kind, "npc.role_changed");
    }
}
