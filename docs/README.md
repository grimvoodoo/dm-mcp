# dm-mcp — Design Documentation

This directory captures every architectural decision made for `dm-mcp`. Each file covers one focused area and explains **what was decided**, **why**, and **what was deferred**.

Read in the order below for a complete picture; or jump to the area you're modifying.

## Foundations

- [Architecture](architecture.md) — Transport layer, MCP SDK, CLI, deployment targets, latency commitments, environment-variable configuration.
- [History & event log](history-log.md) — The polymorphic `events` table that ties every other subsystem together. Two-tier time model. Query patterns.
- [Data persistence & content](content.md) — SQLite layout, bundled YAML content directory structure, content hot-swap via `DMMCP_CONTENT_DIR`.
- [IP & licensing](ip-and-licensing.md) — Hard rules around SRD/ORC-licensed content and avoiding WotC trademarks.

## Entities & mechanics

- [Characters, parties & death](characters.md) — The unified character table that backs the player, companions, pets, and every NPC. Parties. Loyalty. Ideology. Plans. Progression. The three-strike death flow.
- [Checks, effects & conditions](checks.md) — `resolve_check` composition. Temporary effects (never mutate base stats). Conditions with mechanical riders. Proficiencies. The ideology-alignment modifier rubric.
- [Items & inventory](items.md) — Single-table item model. Materials and enchantments as layers. Weight + encumbrance enforcement. Currency as items. Barter.

## World

- [World, zones & maps](world.md) — Graph-of-zones model. Fog of war. Lazy generation. Dungeon nesting. Forward compatibility with hex grids.
- [Encounters & combat](encounters.md) — Goal-not-kills XP. Multiple resolution paths. Combat as a mode of an encounter. Stale-combat cleanup. Initiative.
- [NPC generation](npcs.md) — Archetypes as role+faction, not species essence. Backstory synthesis with reconciliation. Non-violent resolution as a first-class design principle.
- [Campaign setup](campaign-setup.md) — The bootstrap phase: player preferences, world generation, pre-history seeding.
