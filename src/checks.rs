//! Skill / save / attack / ability check resolution.
//!
//! This is the single most frequently-called tool in the dm-mcp toolbox, so the pipeline
//! is laid out linearly with every contribution labelled so the DM agent (and the event
//! log) gets a full breakdown. See `docs/checks.md` for the design.
//!
//! The composition order is:
//!
//! 1. Base ability score (effective — includes active ability-score effects).
//! 2. Proficiency contribution (prof_bonus × (1 + expertise) × proficient + ranks).
//! 3. Flat modifier effects whose `target_key` matches this check's key.
//! 4. Dice effects (e.g. Bless's `1d4`) — each rolled per check, recorded individually.
//! 5. Caller-supplied modifiers (ideology_alignment, hostile_trigger, situational, ...).
//!
//! Advantage / disadvantage is accumulated from: active condition riders on the character's
//! `self` side, and caller-supplied hints. Per 5e-style rules one advantage cancels one
//! disadvantage and any residual gives the effective posture.
//!
//! Auto-fail (from conditions like paralyzed that auto-fail STR/DEX saves) short-circuits
//! the roll and records the outcome explicitly.

use anyhow::{bail, Context, Result};
use rand::RngExt;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::content::{CheckKind, Content, RollModifier};
use crate::dice;
use crate::events::{self, EventSpec, Participant};

// ── Tool params / response ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ResolveCheckParams {
    pub character_id: i64,
    /// One of: `skill_check`, `save`, `attack_roll`, `ability_check`.
    pub kind: String,
    /// The specific check key. Examples by kind:
    /// - skill_check: `persuasion`, `stealth`, `perception`
    /// - save: `save:str`, `save:dex`, ..., `save:cha`
    /// - attack_roll: `attack` (generic) or a weapon base_kind (`longsword`)
    /// - ability_check: ability id — `str`, `dex`, `con`, `int`, `wis`, `cha`
    pub target_key: String,
    /// Optional override for which ability's modifier to apply. If omitted, derived from
    /// the target_key: saves extract the ability from `save:<ability>`, ability_checks use
    /// the target_key directly, skill_checks look up the skill's default ability from
    /// content, attack_rolls default to STR (caller should override for DEX weapons).
    #[serde(default)]
    pub ability: Option<String>,
    /// Optional character-id of whoever the check is against (a target NPC, a DC-setter).
    /// Recorded on the event but not used in the roll.
    #[serde(default)]
    pub target_character_id: Option<i64>,
    /// Difficulty class / AC. If omitted, the tool still rolls but the `success` field is
    /// `None`.
    #[serde(default)]
    pub dc: Option<i32>,
    /// Caller-supplied modifiers. See `docs/checks.md` for the taxonomy.
    #[serde(default)]
    pub modifiers: Vec<NamedModifier>,
    /// Caller-supplied advantage / disadvantage hints (fold into the condition riders).
    #[serde(default)]
    pub advantage: Option<bool>,
    #[serde(default)]
    pub disadvantage: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NamedModifier {
    /// Free-text kind, e.g. `ideology_alignment`, `hostile_trigger`, `loyalty`,
    /// `situational`, `cover`, `flanking`.
    pub kind: String,
    pub value: i32,
    /// Human-readable justification — recorded verbatim on the event payload.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BreakdownEntry {
    pub kind: String,
    pub value: i32,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DieRoll {
    pub spec: String,
    pub value: i64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Posture {
    Advantage,
    Disadvantage,
    Normal,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub character_id: i64,
    pub kind: String,
    pub target_key: String,
    pub ability: String,
    pub posture: Posture,
    /// All d20s rolled (two for adv/dis, one otherwise). `d20_used` is the one kept.
    pub d20s: Vec<i64>,
    pub d20_used: i64,
    /// Additional dice from effects' `dice_expr` (e.g. Bless's 1d4).
    pub effect_dice: Vec<DieRoll>,
    pub total: i64,
    pub dc: Option<i32>,
    pub success: Option<bool>,
    pub crit: bool,
    pub fumble: bool,
    pub auto_fail: bool,
    pub breakdown: Vec<BreakdownEntry>,
    pub event_id: i64,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

const ABILITY_IDS: &[&str] = &["str", "dex", "con", "int", "wis", "cha"];

fn parse_kind(s: &str) -> Result<CheckKind> {
    match s {
        "skill_check" => Ok(CheckKind::SkillCheck),
        "save" | "saving_throw" => Ok(CheckKind::SavingThrow),
        "attack_roll" | "attack" => Ok(CheckKind::AttackRoll),
        "ability_check" => Ok(CheckKind::AbilityCheck),
        _ => bail!(
            "unknown check kind {s:?}; expected one of skill_check, save, attack_roll, ability_check"
        ),
    }
}

/// Resolve which ability's modifier to apply. Rules:
/// - caller override wins if valid
/// - save: target_key is `save:<ability>`, extract the ability
/// - ability_check: target_key is the ability id directly
/// - skill_check: look up the skill's default ability from content
/// - attack_roll: default STR (caller should override for ranged / finesse)
fn resolve_ability(
    content: &Content,
    kind: CheckKind,
    target_key: &str,
    ability_override: Option<&str>,
) -> Result<String> {
    if let Some(a) = ability_override {
        if !ABILITY_IDS.contains(&a) {
            bail!("unknown ability override {a:?}; valid: {ABILITY_IDS:?}");
        }
        return Ok(a.to_string());
    }
    match kind {
        CheckKind::SavingThrow => {
            let a = target_key.strip_prefix("save:").ok_or_else(|| {
                anyhow::anyhow!(
                    "save target_key must be of the form 'save:<ability>'; got {target_key:?}"
                )
            })?;
            if !ABILITY_IDS.contains(&a) {
                bail!("save target_key references unknown ability {a:?}");
            }
            Ok(a.to_string())
        }
        CheckKind::AbilityCheck => {
            if !ABILITY_IDS.contains(&target_key) {
                bail!("ability_check target_key must be an ability id; got {target_key:?}");
            }
            Ok(target_key.to_string())
        }
        CheckKind::SkillCheck => content
            .skills
            .iter()
            .find(|s| s.id == target_key)
            .map(|s| s.ability.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown skill {target_key:?}")),
        CheckKind::AttackRoll => Ok("str".to_string()),
    }
}

/// Per-effect contribution to a composed ability score. Surfaces the `source` so the
/// breakdown can attribute the change rather than just showing the post-composition
/// effective score in the `ability:<x>` line.
#[derive(Debug, Clone)]
struct AbilityEffectContribution {
    source: String,
    modifier: i32,
}

/// Compose an ability score: base from the character row + every active effect targeting
/// that ability column. Returns the composed score and the list of contributing effects
/// (so the caller can itemise them on the breakdown — see issue #17).
fn effective_ability_score(
    conn: &Connection,
    character_id: i64,
    ability: &str,
) -> Result<(i32, Vec<AbilityEffectContribution>)> {
    let col = match ability {
        "str" => "str_score",
        "dex" => "dex_score",
        "con" => "con_score",
        "int" => "int_score",
        "wis" => "wis_score",
        "cha" => "cha_score",
        other => bail!("unknown ability {other:?}"),
    };
    let base: i32 = conn
        .query_row(
            &format!("SELECT {col} FROM characters WHERE id = ?1"),
            [character_id],
            |row| row.get(0),
        )
        .with_context(|| format!("read {col} for character {character_id}"))?;

    let mut stmt = conn.prepare(
        "SELECT source, modifier FROM effects
         WHERE target_character_id = ?1 AND active = 1 AND target_key = ?2",
    )?;
    let contributions: Vec<AbilityEffectContribution> = stmt
        .query_map(rusqlite::params![character_id, col], |row| {
            Ok(AbilityEffectContribution {
                source: row.get(0)?,
                modifier: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let modifier_sum: i32 = contributions.iter().map(|c| c.modifier).sum();
    Ok((base + modifier_sum, contributions))
}

fn ability_modifier(score: i32) -> i32 {
    // Mathematical floor of (score - 10) / 2. div_euclid gives floored division in Rust.
    (score - 10).div_euclid(2)
}

fn load_proficiency_row(
    conn: &Connection,
    character_id: i64,
    name: &str,
) -> Result<Option<(bool, bool, i32)>> {
    let row: Option<(i64, i64, i32)> = conn
        .query_row(
            "SELECT proficient, expertise, ranks FROM character_proficiencies
             WHERE character_id = ?1 AND name = ?2",
            rusqlite::params![character_id, name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional_ok()?;
    Ok(row.map(|(p, e, r)| (p != 0, e != 0, r)))
}

fn proficiency_bonus(conn: &Connection, character_id: i64) -> Result<i32> {
    conn.query_row(
        "SELECT proficiency_bonus FROM characters WHERE id = ?1",
        [character_id],
        |row| row.get(0),
    )
    .context("read proficiency_bonus")
}

#[derive(Debug)]
struct RelevantEffect {
    source: String,
    modifier: i32,
    dice_expr: Option<String>,
}

fn load_effects_for_key(
    conn: &Connection,
    character_id: i64,
    target_key: &str,
) -> Result<Vec<RelevantEffect>> {
    let mut stmt = conn.prepare(
        "SELECT source, modifier, dice_expr FROM effects
         WHERE target_character_id = ?1 AND active = 1 AND target_key = ?2",
    )?;
    let rows: Vec<RelevantEffect> = stmt
        .query_map(rusqlite::params![character_id, target_key], |row| {
            Ok(RelevantEffect {
                source: row.get(0)?,
                modifier: row.get(1)?,
                dice_expr: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn load_active_condition_names(conn: &Connection, character_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT condition FROM character_conditions
         WHERE character_id = ?1 AND active = 1",
    )?;
    let rows: Vec<String> = stmt
        .query_map([character_id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// ── Core: resolve ─────────────────────────────────────────────────────────────

/// Resolve a check. Reads all relevant state from `conn`, composes the roll, emits a
/// `check.resolve` event, and returns the full breakdown.
pub fn resolve(
    conn: &mut Connection,
    content: &Content,
    p: ResolveCheckParams,
) -> Result<CheckResult> {
    let kind = parse_kind(&p.kind)?;

    let ability = resolve_ability(content, kind, &p.target_key, p.ability.as_deref())?;
    let (effective_score, ability_effect_contribs) =
        effective_ability_score(conn, p.character_id, &ability)?;
    let ability_mod = ability_modifier(effective_score);

    // Proficiency row lookup. For skill_check/save/attack_roll we consult the
    // character_proficiencies table keyed by the target_key (or, for attack_roll with a
    // weapon, the weapon's base_kind). For ability_check we skip proficiency entirely.
    let prof = match kind {
        CheckKind::AbilityCheck => None,
        _ => load_proficiency_row(conn, p.character_id, &p.target_key)?,
    };
    let prof_bonus = proficiency_bonus(conn, p.character_id)?;
    let (prof_contrib, prof_expl, ranks) = match prof {
        None => (0, "no proficiency".to_string(), 0),
        Some((proficient, expertise, ranks)) => {
            let base = if proficient {
                prof_bonus * if expertise { 2 } else { 1 }
            } else {
                0
            };
            let total = base + ranks;
            let expl = match (proficient, expertise, ranks) {
                (true, true, r) if r != 0 => format!(
                    "proficient + expertise (+{}) + ranks ({:+})",
                    prof_bonus * 2,
                    r
                ),
                (true, true, _) => format!("proficient + expertise (+{})", prof_bonus * 2),
                (true, false, r) if r != 0 => {
                    format!("proficient (+{}) + ranks ({:+})", prof_bonus, r)
                }
                (true, false, _) => format!("proficient (+{})", prof_bonus),
                (false, _, r) if r != 0 => format!("ranks ({:+})", r),
                _ => "known, not proficient".to_string(),
            };
            (total, expl, ranks)
        }
    };

    // Effect modifiers targeted at this exact check key.
    let effects = load_effects_for_key(conn, p.character_id, &p.target_key)?;
    let effect_flat_sum: i32 = effects.iter().map(|e| e.modifier).sum();

    // Roll each effect's dice_expr separately so the breakdown shows each contribution.
    let mut effect_dice: Vec<DieRoll> = Vec::new();
    for e in &effects {
        if let Some(expr) = &e.dice_expr {
            let roll = dice::roll(expr).with_context(|| {
                format!("roll effect dice_expr {expr:?} (source {:?})", e.source)
            })?;
            effect_dice.push(DieRoll {
                spec: format!("{}/{}", e.source, roll.spec),
                value: roll.total,
            });
        }
    }
    let effect_dice_sum: i64 = effect_dice.iter().map(|d| d.value).sum();

    // Condition riders: self-side.
    let active_conditions = load_active_condition_names(conn, p.character_id)?;
    let mut has_advantage_from_conditions = false;
    let mut has_disadvantage_from_conditions = false;
    let mut auto_fail = false;

    for cond in &active_conditions {
        match content.self_rider_for(cond, kind) {
            Some(RollModifier::Advantage) => has_advantage_from_conditions = true,
            Some(RollModifier::Disadvantage) => has_disadvantage_from_conditions = true,
            Some(RollModifier::AutoFail) => auto_fail = true,
            Some(RollModifier::AutoSucceed) | None => {}
        }
        if content.condition_auto_fails(cond, &p.target_key) {
            auto_fail = true;
        }
    }

    let caller_adv = p.advantage.unwrap_or(false);
    let caller_dis = p.disadvantage.unwrap_or(false);

    // Combine advantage and disadvantage — 5e-style, they cancel exactly once.
    let any_advantage = has_advantage_from_conditions || caller_adv;
    let any_disadvantage = has_disadvantage_from_conditions || caller_dis;
    let posture = match (any_advantage, any_disadvantage) {
        (true, false) => Posture::Advantage,
        (false, true) => Posture::Disadvantage,
        _ => Posture::Normal,
    };

    // Roll d20(s).
    let mut rng = rand::rng();
    let d20s: Vec<i64> = match posture {
        Posture::Normal => vec![rng.random_range(1..=20)],
        _ => vec![rng.random_range(1..=20), rng.random_range(1..=20)],
    };
    let d20_used: i64 = match posture {
        Posture::Normal => d20s[0],
        Posture::Advantage => *d20s.iter().max().unwrap(),
        Posture::Disadvantage => *d20s.iter().min().unwrap(),
    };

    let caller_sum: i32 = p.modifiers.iter().map(|m| m.value).sum();

    let total = d20_used
        + i64::from(ability_mod)
        + i64::from(prof_contrib)
        + i64::from(effect_flat_sum)
        + effect_dice_sum
        + i64::from(caller_sum);

    let (success, total_final) = if auto_fail {
        (Some(false), total)
    } else if let Some(dc) = p.dc {
        (Some(total >= i64::from(dc)), total)
    } else {
        (None, total)
    };

    // Crit / fumble — conventional attack-roll semantics but exposed for any check.
    let crit = matches!(kind, CheckKind::AttackRoll) && d20_used == 20;
    let fumble = matches!(kind, CheckKind::AttackRoll) && d20_used == 1;

    // Breakdown: one entry per contribution. Caller modifiers keep their full tuple so the
    // agent can cite the reason verbatim in narration.
    let mut breakdown = vec![
        BreakdownEntry {
            kind: format!("d20:{}", posture_label(posture)),
            value: d20_used as i32,
            reason: Some(format!("rolled {d20s:?}")),
        },
        BreakdownEntry {
            kind: format!("ability:{ability}"),
            value: ability_mod,
            reason: Some(format!(
                "mod from effective {ability_up} score {effective_score}",
                ability_up = ability.to_ascii_uppercase(),
            )),
        },
    ];

    // Itemise every active effect that contributed to the composed ability score
    // (issue #17). Distinct kind prefix `effect:ability:` so a consumer can tell these
    // annotation entries apart from `effect:<source>` entries (which contribute to the
    // total via effect_flat_sum). The ability:<x> line above already accounts for the
    // composed mod — these per-effect lines exist purely to attribute the source.
    for c in &ability_effect_contribs {
        if c.modifier != 0 {
            breakdown.push(BreakdownEntry {
                kind: format!("effect:ability:{}", c.source),
                value: c.modifier,
                reason: Some(format!(
                    "{source} → {modifier:+} {ability_up} (folded into effective score above)",
                    source = c.source,
                    modifier = c.modifier,
                    ability_up = ability.to_ascii_uppercase(),
                )),
            });
        }
    }

    if prof_contrib != 0 || ranks != 0 || prof.is_some() {
        breakdown.push(BreakdownEntry {
            kind: "proficiency".into(),
            value: prof_contrib,
            reason: Some(prof_expl),
        });
    }
    if effect_flat_sum != 0 {
        for e in &effects {
            if e.modifier != 0 {
                breakdown.push(BreakdownEntry {
                    kind: format!("effect:{}", e.source),
                    value: e.modifier,
                    reason: None,
                });
            }
        }
    }
    for d in &effect_dice {
        breakdown.push(BreakdownEntry {
            kind: format!("effect_die:{}", d.spec),
            value: d.value as i32,
            reason: None,
        });
    }
    for m in &p.modifiers {
        breakdown.push(BreakdownEntry {
            kind: m.kind.clone(),
            value: m.value,
            reason: m.reason.clone(),
        });
    }

    // Event: full breakdown, all rolls, caller modifiers, auto-fail flag.
    let caller_modifiers_payload: Vec<_> = p
        .modifiers
        .iter()
        .map(|m| {
            serde_json::json!({
                "kind": m.kind,
                "value": m.value,
                "reason": m.reason,
            })
        })
        .collect();

    let mut participants = vec![Participant {
        character_id: p.character_id,
        role: "actor",
    }];
    if let Some(t) = p.target_character_id {
        participants.push(Participant {
            character_id: t,
            role: "target",
        });
    }

    let emitted = events::emit(
        conn,
        &EventSpec {
            kind: "check.resolve",
            campaign_hour: 0,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "{kind_label} {target_key} by character id={char_id}: rolled {d20_used} (posture {posture_label}), total {total} vs DC {dc:?} → {success_label}",
                kind_label = p.kind,
                target_key = p.target_key,
                char_id = p.character_id,
                posture_label = posture_label(posture),
                dc = p.dc,
                success_label = match success { Some(true) => "success", Some(false) => "failure", None => "no DC" }
            ),
            payload: serde_json::json!({
                "character_id": p.character_id,
                "kind": p.kind,
                "target_key": p.target_key,
                "target_character_id": p.target_character_id,
                "ability": ability,
                "effective_ability_score": effective_score,
                "ability_mod": ability_mod,
                "proficiency_contrib": prof_contrib,
                "posture": posture,
                "d20s": d20s,
                "d20_used": d20_used,
                "effect_flat_sum": effect_flat_sum,
                "effect_dice": effect_dice.iter().map(|d| serde_json::json!({
                    "spec": d.spec, "value": d.value
                })).collect::<Vec<_>>(),
                "modifiers": caller_modifiers_payload,
                "dc": p.dc,
                "total": total_final,
                "success": success,
                "crit": crit,
                "fumble": fumble,
                "auto_fail": auto_fail,
            }),
            participants: &participants,
            items: &[],
        },
    )?;

    Ok(CheckResult {
        character_id: p.character_id,
        kind: p.kind,
        target_key: p.target_key,
        ability,
        posture,
        d20s,
        d20_used,
        effect_dice,
        total: total_final,
        dc: p.dc,
        success,
        crit,
        fumble,
        auto_fail,
        breakdown,
        event_id: emitted.event_id,
    })
}

fn posture_label(p: Posture) -> &'static str {
    match p {
        Posture::Normal => "normal",
        Posture::Advantage => "advantage",
        Posture::Disadvantage => "disadvantage",
    }
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
    use crate::conditions::{self, ApplyConditionParams};
    use crate::db::schema;
    use crate::effects::{self, ApplyParams as EffectApplyParams};
    use crate::proficiencies::{self, SetProficiencyParams};

    fn fresh() -> (Connection, Content) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        (conn, Content::load(None).unwrap())
    }

    fn make_char(conn: &mut Connection, str_score: i32) -> i64 {
        characters::create(
            conn,
            CreateParams {
                name: "K".into(),
                role: "player".into(),
                str_score,
                dex_score: 10,
                con_score: 10,
                int_score: 10,
                wis_score: 10,
                cha_score: 14,
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
    fn ability_modifier_rules() {
        assert_eq!(ability_modifier(10), 0);
        assert_eq!(ability_modifier(11), 0);
        assert_eq!(ability_modifier(12), 1);
        assert_eq!(ability_modifier(13), 1);
        assert_eq!(ability_modifier(14), 2);
        assert_eq!(ability_modifier(18), 4);
        assert_eq!(ability_modifier(9), -1);
        assert_eq!(ability_modifier(8), -1);
        assert_eq!(ability_modifier(7), -2);
    }

    #[test]
    fn skill_check_uses_content_ability_mapping() {
        let (mut conn, content) = fresh();
        let c = make_char(&mut conn, 10);
        // persuasion's ability is CHA; char has CHA 14, mod +2. No prof. d20 + 2.
        let result = resolve(
            &mut conn,
            &content,
            ResolveCheckParams {
                character_id: c,
                kind: "skill_check".into(),
                target_key: "persuasion".into(),
                ability: None,
                target_character_id: None,
                dc: None,
                modifiers: vec![],
                advantage: None,
                disadvantage: None,
            },
        )
        .unwrap();
        assert_eq!(result.ability, "cha");
        assert!((3..=22).contains(&result.total));
        assert_eq!(result.d20s.len(), 1);
    }

    #[test]
    fn proficiency_adds_prof_bonus() {
        let (mut conn, content) = fresh();
        let c = make_char(&mut conn, 10);
        proficiencies::set_proficiency(
            &mut conn,
            SetProficiencyParams {
                character_id: c,
                name: "persuasion".into(),
                proficient: Some(true),
                expertise: None,
                ranks: None,
            },
        )
        .unwrap();
        // prof_bonus defaults to 2; expected total in [1+2+2, 20+2+2] = [5, 24].
        let result = resolve(
            &mut conn,
            &content,
            ResolveCheckParams {
                character_id: c,
                kind: "skill_check".into(),
                target_key: "persuasion".into(),
                ability: None,
                target_character_id: None,
                dc: None,
                modifiers: vec![],
                advantage: None,
                disadvantage: None,
            },
        )
        .unwrap();
        assert!(
            result.total >= 5 && result.total <= 24,
            "total {} out of expected [5, 24]; breakdown {:?}",
            result.total,
            result.breakdown
        );
    }

    #[test]
    fn bless_on_persuasion_adds_a_rolled_die() {
        let (mut conn, content) = fresh();
        let c = make_char(&mut conn, 10);
        effects::apply(
            &mut conn,
            EffectApplyParams {
                target_character_id: c,
                source: "spell:bless".into(),
                target_kind: "skill".into(),
                target_key: "persuasion".into(),
                modifier: 0,
                dice_expr: Some("1d4".into()),
                expires_at_hour: None,
                expires_after_rounds: None,
                expires_on_dispel: None,
            },
        )
        .unwrap();
        let result = resolve(
            &mut conn,
            &content,
            ResolveCheckParams {
                character_id: c,
                kind: "skill_check".into(),
                target_key: "persuasion".into(),
                ability: None,
                target_character_id: None,
                dc: None,
                modifiers: vec![],
                advantage: None,
                disadvantage: None,
            },
        )
        .unwrap();
        assert_eq!(
            result.effect_dice.len(),
            1,
            "Bless should contribute one rolled die; got {:?}",
            result.effect_dice
        );
        let bless_roll = result.effect_dice[0].value;
        assert!(
            (1..=4).contains(&bless_roll),
            "bless die {bless_roll} out of 1..=4"
        );
    }

    #[test]
    fn blinded_gives_disadvantage_on_attack_rolls() {
        let (mut conn, content) = fresh();
        let c = make_char(&mut conn, 14);
        conditions::apply(
            &mut conn,
            &content,
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
        let result = resolve(
            &mut conn,
            &content,
            ResolveCheckParams {
                character_id: c,
                kind: "attack_roll".into(),
                target_key: "attack".into(),
                ability: Some("str".into()),
                target_character_id: None,
                dc: None,
                modifiers: vec![],
                advantage: None,
                disadvantage: None,
            },
        )
        .unwrap();
        assert_eq!(result.posture, Posture::Disadvantage);
        assert_eq!(result.d20s.len(), 2, "disadvantage rolls 2d20");
        let kept = result.d20_used;
        let min_of = *result.d20s.iter().min().unwrap();
        assert_eq!(kept, min_of, "disadvantage keeps the lower d20");
    }

    #[test]
    fn advantage_and_disadvantage_cancel_to_normal() {
        let (mut conn, content) = fresh();
        let c = make_char(&mut conn, 10);
        // Apply blinded (self-disadvantage on attack_rolls) AND pass advantage via caller.
        conditions::apply(
            &mut conn,
            &content,
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
        let result = resolve(
            &mut conn,
            &content,
            ResolveCheckParams {
                character_id: c,
                kind: "attack_roll".into(),
                target_key: "attack".into(),
                ability: None,
                target_character_id: None,
                dc: None,
                modifiers: vec![],
                advantage: Some(true),
                disadvantage: None,
            },
        )
        .unwrap();
        assert_eq!(result.posture, Posture::Normal, "adv + dis cancel");
        assert_eq!(result.d20s.len(), 1);
    }

    #[test]
    fn caller_modifier_lands_in_breakdown_and_event() {
        let (mut conn, content) = fresh();
        let c = make_char(&mut conn, 10);
        let result = resolve(
            &mut conn,
            &content,
            ResolveCheckParams {
                character_id: c,
                kind: "skill_check".into(),
                target_key: "persuasion".into(),
                ability: None,
                target_character_id: None,
                dc: Some(15),
                modifiers: vec![NamedModifier {
                    kind: "ideology_alignment".into(),
                    value: -6,
                    reason: Some("very_misaligned with request".into()),
                }],
                advantage: None,
                disadvantage: None,
            },
        )
        .unwrap();
        let found = result
            .breakdown
            .iter()
            .find(|e| e.kind == "ideology_alignment")
            .expect("ideology_alignment should be in breakdown");
        assert_eq!(found.value, -6);
        assert_eq!(
            found.reason.as_deref(),
            Some("very_misaligned with request")
        );

        // Event payload should include the same modifier with its reason.
        let (payload_json,): (String,) = conn
            .query_row(
                "SELECT payload FROM events WHERE id = ?1",
                [result.event_id],
                |row| Ok((row.get(0)?,)),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
        let mods = payload["modifiers"].as_array().unwrap();
        assert!(
            mods.iter().any(|m| m["kind"] == "ideology_alignment"
                && m["value"] == -6
                && m["reason"] == "very_misaligned with request"),
            "event payload modifiers {mods:?}"
        );
    }

    #[test]
    fn saving_throw_derives_ability_from_key() {
        let (mut conn, content) = fresh();
        let c = make_char(&mut conn, 10);
        let result = resolve(
            &mut conn,
            &content,
            ResolveCheckParams {
                character_id: c,
                kind: "save".into(),
                target_key: "save:cha".into(),
                ability: None,
                target_character_id: None,
                dc: None,
                modifiers: vec![],
                advantage: None,
                disadvantage: None,
            },
        )
        .unwrap();
        assert_eq!(result.ability, "cha");
    }
}
