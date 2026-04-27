//! NPC generation.
//!
//! `npc.generate(archetype, zone_id?, role_override?)` creates a character from a bundled
//! archetype definition: rolled stats, derived HP, chosen name from the species name pool,
//! proficiencies, a rolled loadout, and 3–5 synthesized backstory events at negative
//! `campaign_hour`. Everything happens in a single transaction so a partial row set is
//! never visible to readers.
//!
//! See `docs/npcs.md` for the conceptual model. This module implements the subset that
//! Phase 8 ships; reconciliation (slot-filling from existing world state) is currently
//! best-effort and treated as deferred content authoring — the placeholders in backstory
//! hook templates survive through to the event payload so a later pass (or the DM agent)
//! can fill them in context.

use anyhow::{bail, Context, Result};
use rand::{Rng, RngExt};
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};

use crate::content::{Archetype, ArchetypeLoadoutEntry, Content, NamePool};
use crate::events::{self, EventSpec, Participant};

// ── Tool params / results ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GenerateParams {
    /// Archetype id from `content/npcs/archetypes/`. e.g. `orc_raider`, `village_elder`.
    pub archetype: String,
    /// Optional current zone for the generated NPC.
    #[serde(default)]
    pub zone_id: Option<i64>,
    /// Optional role override — otherwise `role` is taken from the archetype's `role_hint`.
    /// Must be one of `player`, `companion`, `friendly`, `enemy`, `neutral`.
    #[serde(default)]
    pub role_override: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenerateResult {
    pub character_id: i64,
    pub name: String,
    pub role: String,
    pub archetype: String,
    pub species: String,
    pub rolled_hp_max: i32,
    pub rolled_stats: RolledStats,
    pub item_ids: Vec<i64>,
    pub backstory_event_ids: Vec<i64>,
    pub created_event_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RolledStats {
    pub str_score: i32,
    pub dex_score: i32,
    pub con_score: i32,
    pub int_score: i32,
    pub wis_score: i32,
    pub cha_score: i32,
}

// ── Character.recall ─────────────────────────────────────────────────────────
//
// Lives here rather than in characters.rs because its primary use case is the Phase 8
// backstory-recognition flow — the same query path serves real-play recall and synthesized
// backstory event lookup (see docs/npcs.md §Recall and recognition).

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RecallParams {
    pub character_id: i64,
    #[serde(default)]
    pub zone_id: Option<i64>,
    #[serde(default)]
    pub other_character_id: Option<i64>,
    #[serde(default)]
    pub other_item_id: Option<i64>,
    #[serde(default)]
    pub kind_prefix: Option<String>,
    /// Inclusive lower bound on `campaign_hour`. Negative values include pre-campaign
    /// backstory; omit to include all history.
    #[serde(default)]
    pub since_hour: Option<i64>,
    /// Cap on rows returned; defaults to 50.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallResult {
    pub events: Vec<RecalledEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecalledEvent {
    pub event_id: i64,
    pub kind: String,
    pub campaign_hour: i64,
    pub combat_round: Option<i32>,
    pub zone_id: Option<i64>,
    pub encounter_id: Option<i64>,
    pub summary: String,
    pub role: String,
}

// ── Public API ────────────────────────────────────────────────────────────────

pub fn generate(
    conn: &mut Connection,
    content: &Content,
    p: GenerateParams,
) -> Result<GenerateResult> {
    let arch = content
        .archetypes
        .get(&p.archetype)
        .ok_or_else(|| anyhow::anyhow!("unknown archetype {:?}", p.archetype))?;

    // Resolve role: override > archetype role_hint.
    let role = p
        .role_override
        .clone()
        .unwrap_or_else(|| arch.role_hint.clone());
    if !VALID_ROLES.contains(&role.as_str()) {
        bail!(
            "role {:?} is not one of {VALID_ROLES:?} (archetype={:?}, role_override={:?})",
            role,
            p.archetype,
            p.role_override
        );
    }

    // If zone_id supplied, make sure it exists — otherwise the FK would bite inside the
    // transaction, and the error at that point is less clear.
    if let Some(zid) = p.zone_id {
        let exists: bool = conn
            .query_row("SELECT 1 FROM zones WHERE id = ?1", [zid], |_| Ok(true))
            .optional()
            .context("check zone exists")?
            .unwrap_or(false);
        if !exists {
            bail!("zone_id {zid} does not exist");
        }
    }

    // Reserve all RNG decisions up-front — this keeps the random draws deterministic if we
    // later seed the RNG for repeatability, and keeps the transaction body mechanical.
    let mut rng = rand::rng();

    let rolled = RolledStats {
        str_score: roll_stat(&mut rng, arch, "str", 10),
        dex_score: roll_stat(&mut rng, arch, "dex", 10),
        con_score: roll_stat(&mut rng, arch, "con", 10),
        int_score: roll_stat(&mut rng, arch, "int", 10),
        wis_score: roll_stat(&mut rng, arch, "wis", 10),
        cha_score: roll_stat(&mut rng, arch, "cha", 10),
    };
    let con_mod = ability_modifier(rolled.con_score);

    let hp_max = match arch.hp_formula.as_deref() {
        Some(formula) => eval_hp_formula(formula, con_mod, &mut rng)
            .with_context(|| format!("hp_formula {formula:?}"))?,
        None => default_hp_for_role(&role),
    };
    let ac_base = arch.ac_base.unwrap_or(10);
    let speed_ft = arch.speed_ft.unwrap_or(30);

    let name = pick_name(&mut rng, content.name_pools.get(&arch.species))
        .unwrap_or_else(|| format!("{species} NPC", species = arch.species));
    let ideology = pick_one(&mut rng, &arch.ideology_pool);
    let plans = pick_one(&mut rng, &arch.plan_pool);

    // Pick 3..=5 backstory hooks (capped by however many the archetype declares). Hooks
    // without any declared backstory still yield an empty list rather than an error — the
    // character is simply ahistorical for Phase 8 purposes.
    let hook_count = if arch.backstory_hooks.is_empty() {
        0
    } else {
        let low = arch.backstory_hooks.len().min(3);
        let high = arch.backstory_hooks.len().min(5);
        rng.random_range(low..=high)
    };
    let chosen_hooks: Vec<String> = pick_many(&mut rng, &arch.backstory_hooks, hook_count);

    // Independent negative campaign_hour per hook. Each hook lands somewhere in the past
    // 1..=1000 hours so the DM agent can narrate "years ago" loosely.
    let hook_hours: Vec<i64> = (0..chosen_hooks.len())
        .map(|_| -rng.random_range(1..=1000i64))
        .collect();

    // Loadout decisions rolled up-front as well.
    let loadout_draws: Vec<LoadoutDraw> = arch
        .loadout
        .iter()
        .map(|entry| LoadoutDraw {
            rolled: rng.random_range(0.0_f32..1.0),
            quantity: entry
                .quantity_dice
                .as_deref()
                .map(|expr| {
                    let spec = crate::dice::parse(expr)
                        .context(format!("loadout quantity_dice {expr:?}"))?;
                    Ok::<i64, anyhow::Error>(crate::dice::roll_with(&spec, &mut rng).total)
                })
                .transpose()
                .unwrap_or(None),
            entry: entry.clone(),
        })
        .collect();

    // Substitute every ${slot} the archetype can satisfy from its slot_pools (and the
    // engine-reserved ${years_ago}). Slots without a pool entry stay raw for the DM
    // agent — Content::validate enforces full coverage of declared backstory_hooks at
    // load time, so this is only relevant for forward-compat slots.
    let hook_texts: Vec<String> = chosen_hooks
        .iter()
        .map(|h| substitute_slots(h, &arch.slot_pools, &mut rng))
        .collect();

    let species_label = arch.species.clone();

    // Single tx wraps: character row + character.created event + proficiencies + items +
    // backstory events. Any failure rolls everything back.
    let tx = conn.transaction().context("begin npc.generate tx")?;

    let character_id = insert_character_tx(
        &tx,
        &name,
        &role,
        &rolled,
        hp_max,
        ac_base,
        speed_ft,
        &species_label,
        &p.archetype,
        ideology.as_deref(),
        plans.as_deref(),
        p.zone_id,
    )?;

    let created_event = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "character.created",
            campaign_hour: 0,
            combat_round: None,
            zone_id: p.zone_id,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Generated {role} {name:?} from archetype {:?} (id={character_id})",
                p.archetype
            ),
            payload: serde_json::json!({
                "role": role,
                "archetype": p.archetype,
                "species": species_label,
                "name": name,
                "rolled_stats": rolled,
                "hp_max": hp_max,
                "armor_class": ac_base,
                "ideology": ideology,
                "plans": plans,
            }),
            participants: &[Participant {
                character_id,
                role: "actor",
            }],
            items: &[],
        },
    )?;

    for pr in &arch.proficiencies {
        tx.execute(
            "INSERT INTO character_proficiencies
                (character_id, name, proficient, expertise, ranks)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(character_id, name) DO UPDATE SET
                proficient = excluded.proficient,
                expertise  = excluded.expertise,
                ranks      = excluded.ranks",
            params![
                character_id,
                pr.name,
                pr.proficient as i64,
                pr.expertise as i64,
                pr.ranks,
            ],
        )
        .with_context(|| format!("insert proficiency {:?}", pr.name))?;
    }

    let mut item_ids = Vec::new();
    for draw in &loadout_draws {
        if draw.rolled >= draw.entry.chance {
            continue;
        }
        let quantity = draw.quantity.unwrap_or(1).max(1);
        tx.execute(
            "INSERT INTO items
                (base_kind, material, material_tier, quantity,
                 holder_character_id, equipped_slot, created_at_event_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![
                draw.entry.base_kind,
                draw.entry.material,
                draw.entry.material_tier,
                quantity,
                character_id,
                draw.entry.equip,
                created_event.event_id,
            ],
        )
        .with_context(|| format!("insert loadout item {:?}", draw.entry.base_kind))?;
        item_ids.push(tx.last_insert_rowid());
    }

    let mut backstory_event_ids = Vec::new();
    for (hook_text, hour) in hook_texts.iter().zip(hook_hours.iter()) {
        let emitted = events::emit_in_tx(
            &tx,
            &EventSpec {
                kind: "history.backstory",
                campaign_hour: *hour,
                combat_round: None,
                zone_id: p.zone_id,
                encounter_id: None,
                parent_id: Some(created_event.event_id),
                summary: hook_text.clone(),
                payload: serde_json::json!({
                    "archetype": p.archetype,
                    "hook_template": hook_text,
                    "character_id": character_id,
                }),
                participants: &[Participant {
                    character_id,
                    role: "actor",
                }],
                items: &[],
            },
        )?;
        backstory_event_ids.push(emitted.event_id);
    }

    tx.commit().context("commit npc.generate tx")?;

    Ok(GenerateResult {
        character_id,
        name,
        role,
        archetype: p.archetype,
        species: species_label,
        rolled_hp_max: hp_max,
        rolled_stats: rolled,
        item_ids,
        backstory_event_ids,
        created_event_id: created_event.event_id,
    })
}

pub fn recall(conn: &Connection, p: RecallParams) -> Result<RecallResult> {
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let mut sql = String::from(
        "SELECT e.id, e.kind, e.campaign_hour, e.combat_round, e.zone_id, e.encounter_id,
                e.summary, ep.role
         FROM events e
         JOIN event_participants ep ON ep.event_id = e.id
         WHERE ep.character_id = :character_id",
    );
    let mut extra: Vec<(&str, rusqlite::types::Value)> = Vec::new();
    extra.push((
        ":character_id",
        rusqlite::types::Value::Integer(p.character_id),
    ));
    if let Some(zid) = p.zone_id {
        sql.push_str(" AND e.zone_id = :zone_id");
        extra.push((":zone_id", rusqlite::types::Value::Integer(zid)));
    }
    if let Some(other) = p.other_character_id {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM event_participants ep2 \
                          WHERE ep2.event_id = e.id AND ep2.character_id = :other_character_id)",
        );
        extra.push((
            ":other_character_id",
            rusqlite::types::Value::Integer(other),
        ));
    }
    if let Some(other_item) = p.other_item_id {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM event_items ei WHERE ei.event_id = e.id AND ei.item_id = :other_item_id)",
        );
        extra.push((
            ":other_item_id",
            rusqlite::types::Value::Integer(other_item),
        ));
    }
    if let Some(prefix) = &p.kind_prefix {
        sql.push_str(" AND e.kind LIKE :kind_prefix");
        extra.push((
            ":kind_prefix",
            rusqlite::types::Value::Text(format!("{prefix}%")),
        ));
    }
    if let Some(since) = p.since_hour {
        sql.push_str(" AND e.campaign_hour >= :since_hour");
        extra.push((":since_hour", rusqlite::types::Value::Integer(since)));
    }
    sql.push_str(" ORDER BY e.campaign_hour DESC, e.id DESC LIMIT :limit");
    extra.push((":limit", rusqlite::types::Value::Integer(limit)));

    let mut stmt = conn.prepare(&sql).context("prepare recall query")?;
    let named: Vec<(&str, &dyn rusqlite::ToSql)> = extra
        .iter()
        .map(|(k, v)| (*k, v as &dyn rusqlite::ToSql))
        .collect();
    let events: Vec<RecalledEvent> = stmt
        .query_map(named.as_slice(), |row| {
            Ok(RecalledEvent {
                event_id: row.get(0)?,
                kind: row.get(1)?,
                campaign_hour: row.get(2)?,
                combat_round: row.get(3)?,
                zone_id: row.get(4)?,
                encounter_id: row.get(5)?,
                summary: row.get(6)?,
                role: row.get(7)?,
            })
        })
        .context("execute recall query")?
        .collect::<rusqlite::Result<_>>()
        .context("collect recall rows")?;
    Ok(RecallResult { events })
}

// ── Internals ────────────────────────────────────────────────────────────────

const VALID_ROLES: &[&str] = &["player", "companion", "friendly", "enemy", "neutral"];

struct LoadoutDraw {
    rolled: f32,
    quantity: Option<i64>,
    entry: ArchetypeLoadoutEntry,
}

fn roll_stat<R: Rng + ?Sized>(rng: &mut R, arch: &Archetype, ability: &str, default: i32) -> i32 {
    match arch.stats.get(ability) {
        Some([lo, hi]) if lo <= hi => rng.random_range(*lo..=*hi),
        _ => default,
    }
}

fn ability_modifier(score: i32) -> i32 {
    (score - 10).div_euclid(2)
}

fn default_hp_for_role(role: &str) -> i32 {
    match role {
        "enemy" => 12,
        _ => 10,
    }
}

/// Evaluate a tiny HP-formula grammar: `<term> (+ <term>)*` where each term is either a
/// dice spec (`3d8`), a literal integer, or `con_mod` / `con_mod*<int>`. Whitespace-insensitive.
fn eval_hp_formula<R: Rng + ?Sized>(formula: &str, con_mod: i32, rng: &mut R) -> Result<i32> {
    let mut total: i32 = 0;
    for raw in formula.split('+') {
        let term = raw.trim();
        if term.is_empty() {
            continue;
        }
        if let Some(rest) = term.strip_prefix("con_mod") {
            let rest = rest.trim_start();
            if rest.is_empty() {
                total += con_mod;
            } else if let Some(n) = rest.strip_prefix('*') {
                let factor: i32 = n
                    .trim()
                    .parse()
                    .with_context(|| format!("con_mod multiplier {n:?}"))?;
                total += con_mod * factor;
            } else {
                bail!("unrecognised con_mod form {term:?}");
            }
        } else if term.chars().all(|c| c.is_ascii_digit() || c == '-') {
            total += term
                .parse::<i32>()
                .with_context(|| format!("hp constant {term:?}"))?;
        } else {
            // Treat as dice notation. Reject ranges (min-max) — they have no place in HP.
            let spec = crate::dice::parse(term)?;
            let rolled = crate::dice::roll_with(&spec, rng);
            total += rolled.total as i32;
        }
    }
    Ok(total.max(1))
}

fn pick_name<R: Rng + ?Sized>(rng: &mut R, pool: Option<&NamePool>) -> Option<String> {
    let pool = pool?;
    let first = pool.first.first()?.clone();
    let first = pool
        .first
        .get(rng.random_range(0..pool.first.len()))
        .cloned()
        .unwrap_or(first);
    if pool.last.is_empty() {
        Some(first)
    } else {
        let last = pool
            .last
            .get(rng.random_range(0..pool.last.len()))
            .cloned()?;
        Some(format!("{first} {last}"))
    }
}

fn pick_one<R: Rng + ?Sized>(rng: &mut R, pool: &[String]) -> Option<String> {
    if pool.is_empty() {
        None
    } else {
        pool.get(rng.random_range(0..pool.len())).cloned()
    }
}

fn pick_many<R: Rng + ?Sized>(rng: &mut R, pool: &[String], n: usize) -> Vec<String> {
    let mut shuffled: Vec<String> = pool.to_vec();
    for i in (1..shuffled.len()).rev() {
        let j = rng.random_range(0..=i);
        shuffled.swap(i, j);
    }
    shuffled.into_iter().take(n).collect()
}

/// Substitute `${slot}` placeholders in a backstory hook template.
///
/// - `${years_ago}` — reserved, replaced with a random integer 1..=50.
/// - Any other `${name}` — looked up in `slot_pools[name]`; if present, replaced with
///   one randomly picked entry. Missing pool means the slot is left verbatim so a
///   later reconciliation pass (or the DM agent) can bind it.
///
/// `Content::validate` enforces that every slot referenced in `backstory_hooks` (other
/// than `years_ago`) has a pool entry, so in normal use the verbatim fallback only
/// fires for forward-compat slots an author hasn't pooled yet.
fn substitute_slots<R: Rng + ?Sized>(
    template: &str,
    slot_pools: &std::collections::BTreeMap<String, Vec<String>>,
    rng: &mut R,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' || chars.peek() != Some(&'{') {
            out.push(c);
            continue;
        }
        chars.next(); // consume '{'
        let mut slot = String::new();
        let mut closed = false;
        while let Some(&nc) = chars.peek() {
            if nc == '}' {
                chars.next();
                closed = true;
                break;
            }
            slot.push(nc);
            chars.next();
        }
        if !closed {
            // Unterminated `${...` — emit the literal we consumed and stop scanning.
            out.push_str("${");
            out.push_str(&slot);
            continue;
        }
        if slot == "years_ago" {
            let years = rng.random_range(1..=50i32);
            out.push_str(&years.to_string());
        } else if let Some(pool) = slot_pools.get(&slot).filter(|v| !v.is_empty()) {
            out.push_str(&pool[rng.random_range(0..pool.len())]);
        } else {
            out.push_str("${");
            out.push_str(&slot);
            out.push('}');
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn insert_character_tx(
    tx: &Transaction<'_>,
    name: &str,
    role: &str,
    stats: &RolledStats,
    hp_max: i32,
    ac: i32,
    speed_ft: i32,
    species: &str,
    archetype: &str,
    ideology: Option<&str>,
    plans: Option<&str>,
    zone_id: Option<i64>,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO characters (
            name, role,
            str_score, dex_score, con_score, int_score, wis_score, cha_score,
            hp_current, hp_max, hp_temp,
            armor_class, speed_ft, initiative_bonus,
            size,
            species, class_or_archetype, ideology, plans,
            loyalty,
            current_zone_id,
            created_at, updated_at
        ) VALUES (
            ?1, ?2,
            ?3, ?4, ?5, ?6, ?7, ?8,
            ?9, ?10, 0,
            ?11, ?12, 0,
            'medium',
            ?13, ?14, ?15, ?16,
            50,
            ?17,
            0, 0
        )",
        params![
            name,
            role,
            stats.str_score,
            stats.dex_score,
            stats.con_score,
            stats.int_score,
            stats.wis_score,
            stats.cha_score,
            hp_max,
            hp_max,
            ac,
            speed_ft,
            species,
            archetype,
            ideology,
            plans,
            zone_id,
        ],
    )
    .context("insert characters row")?;
    Ok(tx.last_insert_rowid())
}

// ── Optional-row helper (same shape as characters.rs) ─────────────────────────

trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> Result<Option<T>> {
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

    fn fresh() -> (Connection, Content) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        (conn, Content::load(None).unwrap())
    }

    #[test]
    fn generate_orc_raider_rolls_in_ranges() {
        let (mut conn, content) = fresh();
        let r = generate(
            &mut conn,
            &content,
            GenerateParams {
                archetype: "orc_raider".into(),
                zone_id: None,
                role_override: None,
            },
        )
        .expect("generate");

        assert_eq!(r.role, "enemy");
        assert_eq!(r.species, "orc");
        assert!(r.rolled_hp_max > 0);
        assert!((14..=18).contains(&r.rolled_stats.str_score));
        assert!((10..=14).contains(&r.rolled_stats.dex_score));
        assert!((14..=18).contains(&r.rolled_stats.con_score));
        assert!((3..=5).contains(&r.backstory_event_ids.len()));

        // All backstory events have negative campaign_hour and reference the NPC.
        for eid in &r.backstory_event_ids {
            let (kind, hour): (String, i64) = conn
                .query_row(
                    "SELECT kind, campaign_hour FROM events WHERE id = ?1",
                    [*eid],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(kind, "history.backstory");
            assert!(hour < 0, "backstory hour should be negative, got {hour}");
            let participant: i64 = conn
                .query_row(
                    "SELECT character_id FROM event_participants WHERE event_id = ?1",
                    [*eid],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(participant, r.character_id);
        }

        // Proficiencies inserted.
        let prof_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM character_proficiencies WHERE character_id = ?1",
                [r.character_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            prof_count >= 3,
            "expected several proficiencies, got {prof_count}"
        );
    }

    #[test]
    fn recall_returns_npc_backstory() {
        let (mut conn, content) = fresh();
        let r = generate(
            &mut conn,
            &content,
            GenerateParams {
                archetype: "orc_raider".into(),
                zone_id: None,
                role_override: None,
            },
        )
        .unwrap();

        let recalled = recall(
            &conn,
            RecallParams {
                character_id: r.character_id,
                zone_id: None,
                other_character_id: None,
                other_item_id: None,
                kind_prefix: Some("history.".into()),
                since_hour: None,
                limit: None,
            },
        )
        .expect("recall");
        assert_eq!(recalled.events.len(), r.backstory_event_ids.len());
        for ev in &recalled.events {
            assert_eq!(ev.kind, "history.backstory");
            assert!(ev.campaign_hour < 0);
        }
    }

    #[test]
    fn unknown_archetype_rejected() {
        let (mut conn, content) = fresh();
        let err = generate(
            &mut conn,
            &content,
            GenerateParams {
                archetype: "not_a_thing".into(),
                zone_id: None,
                role_override: None,
            },
        )
        .expect_err("should reject unknown archetype");
        assert!(format!("{err:#}").contains("unknown archetype"));
    }

    #[test]
    fn role_override_applies() {
        let (mut conn, content) = fresh();
        let r = generate(
            &mut conn,
            &content,
            GenerateParams {
                archetype: "orc_raider".into(),
                zone_id: None,
                role_override: Some("neutral".into()),
            },
        )
        .unwrap();
        assert_eq!(r.role, "neutral");
    }

    #[test]
    fn hp_formula_evaluates() {
        let mut rng = rand::rng();
        // "3d8 + con_mod*3" with con_mod=+2 → range 3+6 ..= 24+6 = 9..=30
        let v = eval_hp_formula("3d8 + con_mod*3", 2, &mut rng).unwrap();
        assert!((9..=30).contains(&v), "out of range: {v}");
        // Pure constant.
        let v = eval_hp_formula("17", 0, &mut rng).unwrap();
        assert_eq!(v, 17);
        // Bare con_mod without multiplier.
        let v = eval_hp_formula("10 + con_mod", 3, &mut rng).unwrap();
        assert_eq!(v, 13);
    }

    #[test]
    fn village_elder_is_friendly() {
        let (mut conn, content) = fresh();
        let r = generate(
            &mut conn,
            &content,
            GenerateParams {
                archetype: "village_elder".into(),
                zone_id: None,
                role_override: None,
            },
        )
        .unwrap();
        assert_eq!(r.role, "friendly");
        assert_eq!(r.species, "human");
    }
}
