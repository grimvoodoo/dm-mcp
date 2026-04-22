//! Bundled YAML content loaded once at startup.
//!
//! Per the architectural principle: **content is data, not code**. Everything that describes
//! *what a thing is* (ability definitions, conditions, archetypes, item base-kinds) lives
//! under `content/` and ships inside the binary via `include_str!`. Only instance data lives
//! in SQLite.
//!
//! See:
//!   - `docs/content.md`           — directory layout and loading contract
//!   - `docs/ip-and-licensing.md`  — licensing rules for content authoring
//!
//! `DMMCP_CONTENT_DIR` (if set) loads from disk instead of the embedded copy, for dev
//! iteration or user customisation without rebuilding.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Embedded content ──────────────────────────────────────────────────────────
//
// Files are bundled at compile time. The paths are relative to this source file (Cargo
// rewrites them to be relative to CARGO_MANIFEST_DIR when it applies the macro).

const ABILITIES_YAML: &str = include_str!("../content/rules/abilities.yaml");
const SKILLS_YAML: &str = include_str!("../content/rules/skills.yaml");
const DAMAGE_TYPES_YAML: &str = include_str!("../content/rules/damage_types.yaml");
const CONDITIONS_YAML: &str = include_str!("../content/rules/conditions.yaml");
const BIOMES_YAML: &str = include_str!("../content/world/biomes.yaml");
const WEAPONS_YAML: &str = include_str!("../content/items/bases/weapons.yaml");
const ENCHANTMENTS_YAML: &str = include_str!("../content/items/enchantments.yaml");
const VILLAGE_ELDER_YAML: &str = include_str!("../content/npcs/archetypes/village_elder.yaml");

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
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml_ng::Value>,
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
}

impl Content {
    /// Load content. If `override_dir` is `Some`, read every file from that directory
    /// (expecting the same sub-layout as `content/`). Otherwise use the embedded copies.
    pub fn load(override_dir: Option<&Path>) -> Result<Self> {
        match override_dir {
            Some(dir) => Self::load_from_dir(dir),
            None => Self::load_embedded(),
        }
    }

    fn load_embedded() -> Result<Self> {
        let abilities: AbilitiesFile = parse(ABILITIES_YAML, "rules/abilities.yaml (embedded)")?;
        let skills: SkillsFile = parse(SKILLS_YAML, "rules/skills.yaml (embedded)")?;
        let damage_types: DamageTypesFile =
            parse(DAMAGE_TYPES_YAML, "rules/damage_types.yaml (embedded)")?;
        let conditions: ConditionsFile =
            parse(CONDITIONS_YAML, "rules/conditions.yaml (embedded)")?;
        let biomes: BiomesFile = parse(BIOMES_YAML, "world/biomes.yaml (embedded)")?;
        let weapons: WeaponsFile = parse(WEAPONS_YAML, "items/bases/weapons.yaml (embedded)")?;
        let enchantments: EnchantmentsFile =
            parse(ENCHANTMENTS_YAML, "items/enchantments.yaml (embedded)")?;

        let mut archetypes = BTreeMap::new();
        archetypes.insert(
            "village_elder".to_string(),
            parse::<Archetype>(
                VILLAGE_ELDER_YAML,
                "npcs/archetypes/village_elder.yaml (embedded)",
            )?,
        );

        Self::assemble(
            abilities.abilities,
            skills.skills,
            damage_types.damage_types,
            conditions.conditions,
            biomes.biomes,
            weapons.weapons,
            enchantments.enchantments,
            archetypes,
        )
    }

    fn load_from_dir(dir: &Path) -> Result<Self> {
        let abilities: AbilitiesFile = parse_file(&dir.join("rules/abilities.yaml"))?;
        let skills: SkillsFile = parse_file(&dir.join("rules/skills.yaml"))?;
        let damage_types: DamageTypesFile = parse_file(&dir.join("rules/damage_types.yaml"))?;
        let conditions: ConditionsFile = parse_file(&dir.join("rules/conditions.yaml"))?;
        let biomes: BiomesFile = parse_file(&dir.join("world/biomes.yaml"))?;
        let weapons: WeaponsFile = parse_file(&dir.join("items/bases/weapons.yaml"))?;
        let enchantments: EnchantmentsFile = parse_file(&dir.join("items/enchantments.yaml"))?;

        let mut archetypes = BTreeMap::new();
        let archetype_dir = dir.join("npcs/archetypes");
        if archetype_dir.is_dir() {
            for entry in std::fs::read_dir(&archetype_dir)
                .with_context(|| format!("read archetype dir {}", archetype_dir.display()))?
            {
                let entry = entry?;
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("yaml") {
                    let a: Archetype = parse_file(&p)?;
                    archetypes.insert(a.id.clone(), a);
                }
            }
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
        }
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
}

// ── Parse helpers ─────────────────────────────────────────────────────────────

fn parse<T: for<'de> Deserialize<'de>>(source: &str, label: &str) -> Result<T> {
    serde_yaml_ng::from_str(source).with_context(|| format!("parse {label}"))
}

fn parse_file<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Result<T> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_yaml_ng::from_str(&source).with_context(|| format!("parse {}", path.display()))
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
        // Use the real content/ directory as the override — should match the embedded copy.
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let content_dir = repo_root.join("content");
        let c = Content::load(Some(&content_dir)).expect("load from content/");
        assert_eq!(c.abilities.len(), 6);
        assert!(c.archetypes.contains_key("village_elder"));
    }
}
