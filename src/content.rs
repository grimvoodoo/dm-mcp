//! Bundled YAML content loaded once at startup.
//!
//! Per the architectural principle: **content is data, not code**. Everything that describes
//! *what a thing is* (ability definitions, conditions, archetypes, item base-kinds) lives
//! under `content/` and ships inside the binary via `include_dir!`. Only instance data lives
//! in SQLite.
//!
//! See:
//!   - `docs/content.md`           — directory layout and loading contract
//!   - `docs/ip-and-licensing.md`  — licensing rules for content authoring
//!
//! `DMMCP_CONTENT_DIR` (if set) loads from disk instead of the embedded copy, for dev
//! iteration or user customisation without rebuilding.
//!
//! ### Embedded vs on-disk path symmetry
//!
//! Both loading paths enumerate files from the same relative layout (`rules/abilities.yaml`,
//! `npcs/archetypes/*.yaml`, etc.). The embedded path reads from a compile-time `Dir` tree
//! built by [`include_dir!`]; the on-disk path walks the filesystem. A new YAML under
//! `content/npcs/archetypes/` is picked up by both paths automatically — there's no manual
//! `include_str!` list to keep in sync.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use include_dir::{include_dir, Dir};
use serde::{Deserialize, Serialize};

/// Compile-time snapshot of `content/`. Paths inside are relative to the directory root
/// (e.g. `rules/abilities.yaml`, `npcs/archetypes/village_elder.yaml`).
static CONTENT_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/content");

/// File extension is `.yaml` or `.yml`, case-insensitive.
fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(ext) if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml")
    )
}

// ── Section wrappers ──────────────────────────────────────────────────────────
//
// Each YAML file has a single top-level key (e.g. `abilities:`). We deserialize into a
// small wrapper type per file and then strip the envelope.

#[derive(Debug, Deserialize)]
struct AbilitiesFile {
    abilities: Vec<Ability>,
}

#[derive(Debug, Deserialize)]
struct SkillsFile {
    skills: Vec<Skill>,
}

#[derive(Debug, Deserialize)]
struct DamageTypesFile {
    damage_types: Vec<DamageType>,
}

#[derive(Debug, Deserialize)]
struct ConditionsFile {
    // Conditions have heterogeneous per-condition payloads (self/against/severity_levels).
    // Keep the shape raw here — Phase 2 only needs to enumerate the keys; later phases will
    // parse rider details into typed structs.
    conditions: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Deserialize)]
struct BiomesFile {
    biomes: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Deserialize)]
struct WeaponsFile {
    weapons: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Deserialize)]
struct EnchantmentsFile {
    enchantments: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Deserialize)]
struct SetupQuestionsFile {
    questions: Vec<SetupQuestion>,
}

#[derive(Debug, Deserialize)]
struct DeathEventsFile {
    death_events: Vec<DeathEvent>,
}

/// One file under `content/items/bases/` — e.g. `weapons.yaml` has top-level key `weapons`,
/// `general.yaml` has top-level key `general`. The `Content` loader unions the single
/// top-level map from every file into `Content.item_bases`, letting authors group bases
/// into separate YAMLs without a manual list.
#[derive(Debug, Deserialize)]
struct ItemBasesFile(BTreeMap<String, BTreeMap<String, BaseItem>>);

#[derive(Debug, Deserialize)]
struct EncumbranceFile {
    encumbrance: EncumbranceRules,
}

// ── Public content types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ability {
    pub id: String,
    pub name: String,
    pub governs: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub ability: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DamageType {
    pub id: String,
    pub physical: bool,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Archetype {
    pub id: String,
    pub species: String,
    pub role_hint: String,
    /// `[min, max]` years.
    #[serde(default)]
    pub typical_age_years: Option<[i32; 2]>,
    /// `{ str: [14, 18], dex: [10, 14], ... }` — inclusive ranges, per ability id.
    #[serde(default)]
    pub stats: BTreeMap<String, [i32; 2]>,
    /// Dice expression with `con_mod` interpolation. Currently consumed by `npcs::generate`.
    #[serde(default)]
    pub hp_formula: Option<String>,
    #[serde(default)]
    pub ac_base: Option<i32>,
    #[serde(default)]
    pub speed_ft: Option<i32>,
    #[serde(default)]
    pub proficiencies: Vec<ArchetypeProficiency>,
    #[serde(default)]
    pub loadout: Vec<ArchetypeLoadoutEntry>,
    #[serde(default)]
    pub plan_pool: Vec<String>,
    #[serde(default)]
    pub ideology_pool: Vec<String>,
    #[serde(default)]
    pub backstory_hooks: Vec<String>,
    #[serde(default)]
    pub hostile_triggers: Vec<String>,
    #[serde(default)]
    pub peace_hooks: Vec<String>,
    /// Catch-all for forward-compat / agent-readable fields not yet mechanised.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchetypeProficiency {
    pub name: String,
    #[serde(default)]
    pub proficient: bool,
    #[serde(default)]
    pub expertise: bool,
    #[serde(default)]
    pub ranks: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchetypeLoadoutEntry {
    pub base_kind: String,
    /// 0.0..=1.0 — probability the entry produces an item at generation time.
    pub chance: f32,
    #[serde(default)]
    pub material: Option<String>,
    #[serde(default)]
    pub material_tier: Option<i32>,
    /// Optional dice expression (e.g. `2d6`) for stackable items like gold.
    #[serde(default)]
    pub quantity_dice: Option<String>,
    #[serde(default)]
    pub equip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamePool {
    pub species: String,
    #[serde(default)]
    pub first: Vec<String>,
    #[serde(default)]
    pub last: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeathEvent {
    pub kind: String,
    pub weight: i32,
    pub description: String,
    #[serde(default)]
    pub outcome_hooks: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
}

/// Typed view of an item base. Only the fields Phase 10 needs are first-class — other
/// fields carried on the YAML (damage, damage_type, properties, slot) are preserved in
/// `extra` via flatten, so authors can add richer bases without code churn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseItem {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Weight in pounds. Required for encumbrance math; content-authoring convention is
    /// that every base carries one (even non-physical items can set 0).
    #[serde(default)]
    pub weight_lb: f64,
    #[serde(default)]
    pub base_value_gp: f64,
    #[serde(default)]
    pub stackable: bool,
    #[serde(default)]
    pub slot: Option<String>,
    #[serde(default)]
    pub properties: Vec<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncumbranceRules {
    pub capacity_per_str: i32,
    pub encumbered_threshold_pct: i32,
    pub overloaded_threshold_pct: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupQuestion {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub options: Vec<String>,
    /// If true, the player may answer outside the provided options.
    #[serde(default)]
    pub or_free_text: bool,
    /// If true, the answer is an array of selected options.
    #[serde(default)]
    pub multi: bool,
}

/// Parsed content, ready for tool lookups. Held in an `Arc` and shared across MCP sessions.
#[derive(Debug)]
pub struct Content {
    pub abilities: Vec<Ability>,
    pub skills: Vec<Skill>,
    pub damage_types: Vec<DamageType>,
    pub conditions: BTreeMap<String, serde_yaml_ng::Value>,
    pub biomes: BTreeMap<String, serde_yaml_ng::Value>,
    pub weapons: BTreeMap<String, serde_yaml_ng::Value>,
    pub enchantments: BTreeMap<String, serde_yaml_ng::Value>,
    pub archetypes: BTreeMap<String, Archetype>,
    pub name_pools: BTreeMap<String, NamePool>,
    pub setup_questions: Vec<SetupQuestion>,
    pub death_events: Vec<DeathEvent>,
    pub item_bases: BTreeMap<String, BaseItem>,
    pub encumbrance: EncumbranceRules,
}

/// Source of a single YAML file's bytes. Hides the difference between embedded (`&'static
/// str`) and on-disk (`String`) so the parsing code is shared.
enum YamlSource<'a> {
    Embedded { path: &'a str, source: &'a str },
    OnDisk { path: String, source: String },
}

impl YamlSource<'_> {
    fn label(&self) -> String {
        match self {
            YamlSource::Embedded { path, .. } => format!("{path} (embedded)"),
            YamlSource::OnDisk { path, .. } => path.clone(),
        }
    }
    fn text(&self) -> &str {
        match self {
            YamlSource::Embedded { source, .. } => source,
            YamlSource::OnDisk { source, .. } => source,
        }
    }
}

impl Content {
    /// Load content. If `override_dir` is `Some`, read every file from that directory
    /// (expecting the same sub-layout as `content/`). Otherwise use the embedded copies.
    pub fn load(override_dir: Option<&Path>) -> Result<Self> {
        let abilities: AbilitiesFile = parse(&get(override_dir, "rules/abilities.yaml")?)?;
        let skills: SkillsFile = parse(&get(override_dir, "rules/skills.yaml")?)?;
        let damage_types: DamageTypesFile = parse(&get(override_dir, "rules/damage_types.yaml")?)?;
        let conditions: ConditionsFile = parse(&get(override_dir, "rules/conditions.yaml")?)?;
        let biomes: BiomesFile = parse(&get(override_dir, "world/biomes.yaml")?)?;
        let weapons: WeaponsFile = parse(&get(override_dir, "items/bases/weapons.yaml")?)?;
        let enchantments: EnchantmentsFile = parse(&get(override_dir, "items/enchantments.yaml")?)?;
        let setup_questions: SetupQuestionsFile =
            parse(&get(override_dir, "campaign/setup_questions.yaml")?)?;
        let death_events: DeathEventsFile = parse(&get(override_dir, "rules/death_events.yaml")?)?;
        let encumbrance: EncumbranceFile = parse(&get(override_dir, "rules/encumbrance.yaml")?)?;

        // Item bases: every file under items/bases/ contributes to a flat base_kind →
        // BaseItem map. Each file has a single top-level grouping key (e.g. `weapons:` or
        // `general:`) that exists for author semantics only — the inner map is what we
        // keep.
        let mut item_bases: BTreeMap<String, BaseItem> = BTreeMap::new();
        let base_files = list_yaml_files_under(override_dir, "items/bases")?;
        for src in &base_files {
            let wrapper: ItemBasesFile = parse(src)?;
            for (_category, entries) in wrapper.0 {
                for (base_kind, base) in entries {
                    item_bases.insert(base_kind, base);
                }
            }
        }

        // Archetypes: discover every *.yaml / *.yml under npcs/archetypes/ in whichever
        // source we're using. Embedded and on-disk use the same iteration logic so a new
        // archetype file is picked up by both paths automatically.
        let mut archetypes = BTreeMap::new();
        let archetype_sources = list_yaml_files_under(override_dir, "npcs/archetypes")?;
        for src in &archetype_sources {
            let a: Archetype = parse(src)?;
            archetypes.insert(a.id.clone(), a);
        }

        // Name pools: same auto-discovery shape; keyed by `species`.
        let mut name_pools = BTreeMap::new();
        let pool_sources = list_yaml_files_under(override_dir, "npcs/name_pools")?;
        for src in &pool_sources {
            let np: NamePool = parse(src)?;
            name_pools.insert(np.species.clone(), np);
        }

        Self::assemble(
            abilities.abilities,
            skills.skills,
            damage_types.damage_types,
            conditions.conditions,
            biomes.biomes,
            weapons.weapons,
            enchantments.enchantments,
            archetypes,
            name_pools,
            setup_questions.questions,
            death_events.death_events,
            item_bases,
            encumbrance.encumbrance,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        abilities: Vec<Ability>,
        skills: Vec<Skill>,
        damage_types: Vec<DamageType>,
        conditions: BTreeMap<String, serde_yaml_ng::Value>,
        biomes: BTreeMap<String, serde_yaml_ng::Value>,
        weapons: BTreeMap<String, serde_yaml_ng::Value>,
        enchantments: BTreeMap<String, serde_yaml_ng::Value>,
        archetypes: BTreeMap<String, Archetype>,
        name_pools: BTreeMap<String, NamePool>,
        setup_questions: Vec<SetupQuestion>,
        death_events: Vec<DeathEvent>,
        item_bases: BTreeMap<String, BaseItem>,
        encumbrance: EncumbranceRules,
    ) -> Result<Self> {
        let content = Self {
            abilities,
            skills,
            damage_types,
            conditions,
            biomes,
            weapons,
            enchantments,
            archetypes,
            name_pools,
            setup_questions,
            death_events,
            item_bases,
            encumbrance,
        };
        content.validate()?;
        Ok(content)
    }

    /// Cross-section invariants: every skill's `ability` key must match a real ability id.
    /// Future phases will add more checks (archetype proficiencies reference real skills, etc.).
    fn validate(&self) -> Result<()> {
        let ability_ids: std::collections::HashSet<&str> =
            self.abilities.iter().map(|a| a.id.as_str()).collect();
        for s in &self.skills {
            if !ability_ids.contains(s.ability.as_str()) {
                anyhow::bail!(
                    "skill {:?} references unknown ability {:?}; valid abilities: {:?}",
                    s.id,
                    s.ability,
                    ability_ids
                );
            }
        }
        Ok(())
    }

    /// Summary for the `content.introspect` MCP tool. One map per section with its IDs —
    /// small, deterministic, easy for the agent to diff across runs.
    pub fn introspect(&self) -> Introspection {
        Introspection {
            abilities: self.abilities.iter().map(|a| a.id.clone()).collect(),
            skills: self.skills.iter().map(|s| s.id.clone()).collect(),
            damage_types: self.damage_types.iter().map(|d| d.id.clone()).collect(),
            conditions: self.conditions.keys().cloned().collect(),
            biomes: self.biomes.keys().cloned().collect(),
            weapons: self.weapons.keys().cloned().collect(),
            enchantments: self.enchantments.keys().cloned().collect(),
            archetypes: self.archetypes.keys().cloned().collect(),
            name_pools: self.name_pools.keys().cloned().collect(),
            setup_questions: self.setup_questions.iter().map(|q| q.id.clone()).collect(),
            death_events: self.death_events.iter().map(|d| d.kind.clone()).collect(),
            item_bases: self.item_bases.keys().cloned().collect(),
        }
    }
}

// ── Condition rider accessors ──────────────────────────────────────────────────
//
// Phase 5's `resolve_check` needs to know: does condition X impose advantage /
// disadvantage / auto-fail on check kind Y for the character WHO IS affected? We keep the
// raw rider payload as `serde_yaml_ng::Value` in `Content.conditions` so the DM agent can
// read any field we don't yet mechanise — these accessors carve out the narrow set the
// check pipeline acts on.

/// Roll-level modifier on a specific check kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollModifier {
    Advantage,
    Disadvantage,
    AutoFail,
    AutoSucceed,
}

/// Categories of check the rider table may modify. Keep the string form aligned with the
/// YAML keys so content authors don't have to remember a separate enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    AttackRoll,
    AbilityCheck,
    SkillCheck,
    SavingThrow,
}

impl CheckKind {
    /// YAML key this check kind is matched against in the `self` rider block.
    pub fn rider_key(&self) -> &'static str {
        match self {
            CheckKind::AttackRoll => "attack_rolls",
            CheckKind::AbilityCheck => "ability_checks",
            // Skill checks are a sub-class of ability checks mechanically; the rider table
            // can target them via either key.
            CheckKind::SkillCheck => "ability_checks",
            CheckKind::SavingThrow => "saves",
        }
    }
}

impl Content {
    /// Look up the `self.<kind>` rider for the given condition id. Returns `None` if the
    /// condition is unknown or the rider field is absent.
    pub fn self_rider_for(&self, condition: &str, kind: CheckKind) -> Option<RollModifier> {
        let cond = self.conditions.get(condition)?;
        let self_block = cond.get("self")?;
        let val = self_block.get(kind.rider_key())?;
        parse_roll_modifier(val)
    }

    /// Returns true if the given check-key (e.g. `save:str`) is in the condition's
    /// `self.auto_fail` list.
    pub fn condition_auto_fails(&self, condition: &str, check_key: &str) -> bool {
        let Some(cond) = self.conditions.get(condition) else {
            return false;
        };
        let Some(self_block) = cond.get("self") else {
            return false;
        };
        match self_block.get("auto_fail") {
            Some(serde_yaml_ng::Value::Sequence(seq)) => seq
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s == check_key)),
            _ => false,
        }
    }
}

fn parse_roll_modifier(v: &serde_yaml_ng::Value) -> Option<RollModifier> {
    match v.as_str()? {
        "advantage" => Some(RollModifier::Advantage),
        "disadvantage" => Some(RollModifier::Disadvantage),
        "auto_fail" => Some(RollModifier::AutoFail),
        "auto_succeed" => Some(RollModifier::AutoSucceed),
        _ => None,
    }
}

/// Structured summary returned by `content.introspect`. JSON-serialisable so the handler
/// can ship it straight to the MCP client.
#[derive(Debug, Serialize, Deserialize)]
pub struct Introspection {
    pub abilities: Vec<String>,
    pub skills: Vec<String>,
    pub damage_types: Vec<String>,
    pub conditions: Vec<String>,
    pub biomes: Vec<String>,
    pub weapons: Vec<String>,
    pub enchantments: Vec<String>,
    pub archetypes: Vec<String>,
    pub name_pools: Vec<String>,
    pub setup_questions: Vec<String>,
    pub death_events: Vec<String>,
    pub item_bases: Vec<String>,
}

// ── Source lookup ─────────────────────────────────────────────────────────────

/// Fetch a YAML file by its relative path, from the override dir if set, else the embedded
/// bundle. Fails with a clear error if the file is missing.
fn get<'a>(override_dir: Option<&Path>, rel: &'a str) -> Result<YamlSource<'a>> {
    match override_dir {
        Some(dir) => {
            let abs = dir.join(rel);
            let source =
                std::fs::read_to_string(&abs).with_context(|| format!("read {}", abs.display()))?;
            Ok(YamlSource::OnDisk {
                path: abs.display().to_string(),
                source,
            })
        }
        None => {
            let file = CONTENT_DIR
                .get_file(rel)
                .with_context(|| format!("embedded content missing: {rel}"))?;
            let source = file
                .contents_utf8()
                .with_context(|| format!("embedded {rel} is not UTF-8"))?;
            Ok(YamlSource::Embedded { path: rel, source })
        }
    }
}

/// Enumerate every `*.yaml` / `*.yml` under `<content>/<rel_dir>/`. Returns an empty list
/// when the directory is absent — used for both archetype YAMLs and name-pool YAMLs, the
/// latter possibly missing in older content snapshots.
fn list_yaml_files_under<'a>(
    override_dir: Option<&Path>,
    rel_dir: &str,
) -> Result<Vec<YamlSource<'a>>> {
    let mut out = Vec::new();
    match override_dir {
        Some(dir) => {
            let abs = dir.join(rel_dir);
            if abs.is_dir() {
                for entry in std::fs::read_dir(&abs)
                    .with_context(|| format!("read content dir {}", abs.display()))?
                {
                    let entry = entry?;
                    let path = entry.path();
                    if is_yaml_file(&path) {
                        let source = std::fs::read_to_string(&path)
                            .with_context(|| format!("read {}", path.display()))?;
                        out.push(YamlSource::OnDisk {
                            path: path.display().to_string(),
                            source,
                        });
                    }
                }
            }
        }
        None => {
            // Embedded copy may not contain this subdir — treat absence as "no files".
            if let Some(d) = CONTENT_DIR.get_dir(rel_dir) {
                for file in d.files() {
                    let path = file.path();
                    if is_yaml_file(path) {
                        let source = file
                            .contents_utf8()
                            .with_context(|| format!("embedded {} is not UTF-8", path.display()))?;
                        out.push(YamlSource::Embedded {
                            path: static_path_str(path),
                            source,
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Leak a Path's string into a `'static` slice via Box::leak. Only called once per embedded
/// archetype file at startup — fine for a bounded, small content set.
fn static_path_str(p: &Path) -> &'static str {
    Box::leak(p.display().to_string().into_boxed_str())
}

// ── Parse helpers ─────────────────────────────────────────────────────────────

fn parse<T: for<'de> Deserialize<'de>>(src: &YamlSource<'_>) -> Result<T> {
    serde_yaml_ng::from_str(src.text()).with_context(|| format!("parse {}", src.label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_content_loads_and_validates() {
        let c = Content::load(None).expect("embedded content should parse");
        assert_eq!(c.abilities.len(), 6, "six ability scores expected");
        assert_eq!(c.skills.len(), 18, "18 skills expected");
        assert!(
            c.damage_types.iter().any(|d| d.id == "fire"),
            "fire damage type should be present"
        );
        assert!(c.conditions.contains_key("blinded"));
        assert!(c.weapons.contains_key("longsword"));
        assert!(c.enchantments.contains_key("glowing"));
        assert!(c.biomes.contains_key("temperate_forest"));
        assert!(c.archetypes.contains_key("village_elder"));
    }

    #[test]
    fn introspection_returns_every_section() {
        let c = Content::load(None).expect("load");
        let s = c.introspect();
        assert!(s.abilities.contains(&"str".to_string()));
        assert!(s.skills.contains(&"stealth".to_string()));
        assert!(s.damage_types.contains(&"necrotic".to_string()));
        assert!(s.conditions.contains(&"paralyzed".to_string()));
        assert!(s.weapons.contains(&"longsword".to_string()));
        assert!(s.enchantments.contains(&"glowing".to_string()));
        assert!(s.biomes.contains(&"temperate_forest".to_string()));
        assert!(s.archetypes.contains(&"village_elder".to_string()));
    }

    #[test]
    fn validate_rejects_skill_with_unknown_ability() {
        // Build a Content by hand with a bogus skill reference.
        let content = Content {
            abilities: vec![Ability {
                id: "str".into(),
                name: "Strength".into(),
                governs: "...".into(),
            }],
            skills: vec![Skill {
                id: "bogus".into(),
                ability: "xyz".into(), // <- not a real ability id
                description: "...".into(),
            }],
            damage_types: vec![],
            conditions: BTreeMap::new(),
            biomes: BTreeMap::new(),
            weapons: BTreeMap::new(),
            enchantments: BTreeMap::new(),
            archetypes: BTreeMap::new(),
            name_pools: BTreeMap::new(),
            setup_questions: vec![],
            death_events: vec![],
            item_bases: BTreeMap::new(),
            encumbrance: EncumbranceRules {
                capacity_per_str: 15,
                encumbered_threshold_pct: 67,
                overloaded_threshold_pct: 100,
            },
        };
        let err = content
            .validate()
            .expect_err("should reject unknown ability");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown ability"),
            "error should name the broken invariant: {msg}"
        );
    }

    #[test]
    fn dir_override_reads_yaml_from_disk() {
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let content_dir = repo_root.join("content");
        let c = Content::load(Some(&content_dir)).expect("load from content/");
        assert_eq!(c.abilities.len(), 6);
        assert!(c.archetypes.contains_key("village_elder"));
    }

    #[test]
    fn yml_extension_is_also_accepted() {
        // Sanity-check the extension matcher without manipulating the real fixtures.
        assert!(is_yaml_file(Path::new("foo.yaml")));
        assert!(is_yaml_file(Path::new("foo.yml")));
        assert!(is_yaml_file(Path::new("FOO.YAML")));
        assert!(is_yaml_file(Path::new("bar.YML")));
        assert!(!is_yaml_file(Path::new("foo.json")));
        assert!(!is_yaml_file(Path::new("foo.txt")));
        assert!(!is_yaml_file(Path::new("foo")));
    }

    #[test]
    fn override_dir_picks_up_extra_archetype_that_embedded_does_not() {
        // Build a tmp dir that mirrors the real content/, drop in a second archetype, and
        // verify Content::load(Some(&tmp)) discovers it. This is the regression guard
        // against silent embedded/on-disk divergence (CodeRabbit PR #5 review).
        use std::fs;
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let real_content = repo_root.join("content");
        let tmp = tempfile::TempDir::new().expect("tmpdir");

        fn copy_dir(src: &Path, dst: &Path) {
            fs::create_dir_all(dst).unwrap();
            for entry in fs::read_dir(src).unwrap() {
                let entry = entry.unwrap();
                let ft = entry.file_type().unwrap();
                let from = entry.path();
                let to = dst.join(entry.file_name());
                if ft.is_dir() {
                    copy_dir(&from, &to);
                } else if ft.is_file() {
                    fs::copy(&from, &to).unwrap();
                }
            }
        }
        copy_dir(&real_content, tmp.path());

        let extra = tmp.path().join("npcs/archetypes/test_bandit.yaml");
        fs::write(
            &extra,
            "id: test_bandit\nspecies: human\nrole_hint: enemy\n",
        )
        .unwrap();

        let c = Content::load(Some(tmp.path())).expect("load from override");
        assert!(
            c.archetypes.contains_key("test_bandit"),
            "override-dir path should auto-discover new archetype YAML files"
        );
        assert!(c.archetypes.contains_key("village_elder"));
    }
}
