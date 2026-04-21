# Content

## Principle: content in code, instances in the database

Everything that defines **what a thing is** — what a longsword does, what an orc raider looks like, how much exhaustion-level-3 hurts — lives in bundled YAML files inside the `content/` directory. Everything that describes **which specific thing** — this particular sword held by that particular orc in that particular zone — lives in the SQLite database.

This split has several consequences, all deliberate:

- **Rebuildable worlds, customisable campaigns.** A user who wants to tweak material modifiers, add a new enchantment, or rewrite the death-event table edits YAML and rebuilds. A user who wants runtime iteration points `DMMCP_CONTENT_DIR` at an on-disk copy.
- **No content migrations.** The database carries instance data only. Adding a new archetype or rebalancing a weapon does not require schema changes or data migrations — just a content edit.
- **Self-contained binary.** Content is embedded via `include_str!` at compile time. The `scratch` container carries zero runtime file-system dependencies; no mounting of content volumes in the common case.
- **Testability.** Content tables are plain data. Unit tests load them in isolation, validate schemas, catch typos, and check invariants (every archetype has at least one `peace_hook` unless `can_parley: false`; every enchantment has a `value_premium_gp`; and so on).

## Directory layout

```
content/
  rules/
    abilities.yaml             # six ability scores and what they govern
    skills.yaml                # named skills → associated ability
    conditions.yaml            # conditions + mechanical riders
    damage_types.yaml          # slashing, piercing, fire, necrotic, etc.
    encumbrance.yaml           # capacity formula, thresholds
    death_events.yaml          # rolled when a character fully dies
    ideology_alignment.yaml    # the +3 to -10 rubric
    xp_level_table.yaml        # XP → level → proficiency_bonus

  items/
    bases/
      weapons.yaml             # longsword, dagger, greataxe, longbow, ...
      armor.yaml               # leather, mail, plate, shields
      consumables.yaml         # potions, rations, scrolls
      general.yaml             # gold, rope, torches, tools
    materials.yaml             # tier-1 basic → tier-5 exotic; weight, value, damage, durability modifiers
    enchantments.yaml          # glowing, sharpness, goblinbane, scaling and static

  npcs/
    archetypes/                # one file per archetype
      orc_raider.yaml
      orc_merchant.yaml
      village_baker.yaml
      death_cultist.yaml
      hedge_wizard.yaml
      forest_wolf.yaml
      ...
    name_pools/                # per species/culture
      orc.yaml
      human_northern.yaml
      elf_sylvan.yaml
      ...

  world/
    biomes.yaml                # forest, plains, mountains, etc.; encounter-tag pools
    encounters.yaml            # encounter templates with goals, participants, resolution_paths
    dungeon_templates.yaml     # procedural dungeon parameters
    zone_templates/            # per-biome NPC-slot definitions and landmark pools
      wilderness_forest.yaml
      settlement_village_small.yaml
      settlement_city.yaml
      dungeon.yaml

  campaign/
    setup_questions.yaml       # bootstrap dialogue
```

## Authoring rules

### Rules → see related docs

- [Characters, parties & death](characters.md) — for `death_events.yaml` shape
- [Checks, effects & conditions](checks.md) — for `conditions.yaml` rider syntax and `ideology_alignment.yaml` rubric
- [Items & inventory](items.md) — for `weapons.yaml`, `materials.yaml`, `enchantments.yaml` shapes
- [NPC generation](npcs.md) — for archetype YAML shape
- [World, zones & maps](world.md) — for zone and dungeon templates
- [Encounters & combat](encounters.md) — for encounter resolution-paths shape
- [Campaign setup](campaign-setup.md) — for setup-questions shape

### IP-safety rules are hard

Every content file is subject to the [IP & licensing](ip-and-licensing.md) rules. Summary: original text or text derived from openly-licensed sources (5.1 SRD CC-BY-4.0 or ORC-licensed material) only. No copy-paste of WotC product-book text. No "D&D" / "5e" / "Dungeons & Dragons" branding anywhere.

### No alignment axis in archetypes

- No `alignment: evil`, no `good_evil: true`.
- Hostility is expressed through situation, faction, and `hostile_triggers` — not species nature.
- Every hostile encounter must have at least one peaceful `resolution_path`, unless all participants are mindless (`can_parley: false`).

See [NPC generation — non-violent resolution](npcs.md#non-violent-resolution-first-class) for the full rule.

## Loading

At startup, the content loader:

1. Walks the embedded or on-disk content directory.
2. Parses each YAML into typed Rust structs (via `serde`).
3. Validates internal invariants (all archetype weapon proficiencies reference valid weapon base_kinds; all enchantment `effects` reference valid `target_kind` values; etc.).
4. Stores everything in a single `Content` struct held in the application's global state.
5. Exposes indexed accessors (`content.archetype("orc_raider")`, `content.enchantment("sharpness")`, `content.biome("forest")`).

**The loader runs exactly once per process.** Tool-call hot paths never touch disk or parse YAML. Any time a tool says "look up the longsword's base weight," that's an O(1) hash lookup in an in-memory struct.

## Overriding bundled content

Set `DMMCP_CONTENT_DIR=/path/to/content` to load from disk instead of the embedded copy. Two uses:

- **Development iteration.** Edit a YAML, restart the process, see the change — no `cargo build` cycle.
- **End-user customisation.** A user running the container can mount a custom content directory and the image uses it without rebuilding.

The on-disk directory must have the same layout as the bundled one. If required files are missing, the loader fails fast at startup with a clear error.

## When to change schema vs. content

If you find yourself tempted to add a column to a database table to support a new rule, ask: *can this live in content instead?*

- A new status effect? **Content** (`conditions.yaml`).
- A new weapon? **Content** (`weapons.yaml`).
- A new ability score that doesn't exist in d20 systems? Schema change — base stats are on `characters`.
- A fundamentally new entity type (a "building" that isn't a zone or landmark)? Schema change — new table.

Most additions should be content-only. Schema changes are a smell; think twice.
