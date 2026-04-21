# World, zones & maps

## Shape

The world is a **directed graph of zones**. Each zone is a narrative node with a prose description, not a tile map. Hex grids are deferred — possibly forever, certainly for MVP — but the design avoids choices that would fight a later hex layer.

The player moves through the graph zone by zone. What they know of the world is per-character fog of war. Internal structure of a zone (a dungeon's rooms and floors) uses the **same `zones` table**, with `parent_zone_id` giving nested structure.

## Schema

```
zones(
    id                    PK,
    name                  TEXT,
    biome                 TEXT,             -- content key: forest, mountains, settlement, dungeon_stone, ...
    kind                  TEXT,             -- wilderness | settlement | dungeon | dungeon_floor | dungeon_room | road | liminal
    size                  TEXT,             -- tiny | small | medium | large | vast
    parent_zone_id        FK?,              -- nested zones: dungeon → floor → room
    description           TEXT,             -- prose for the agent
    encounter_tags        TEXT,             -- JSON array of tags indexing into encounter content
    created_at_event_id   FK?,
    notes                 TEXT?
)

zone_connections(
    from_zone_id          FK,
    to_zone_id            FK,
    travel_time_hours     INTEGER,          -- world-hours to traverse
    travel_mode           TEXT,             -- road | wilderness | portal | passage | climb
    hazard_tag            TEXT?,            -- biases random-encounter rolls in transit
    one_way               BOOL,             -- waterfalls, one-way portals
    direction_from        TEXT,             -- n | ne | e | se | s | sw | w | nw | up | down
    PRIMARY KEY (from_zone_id, to_zone_id)
)

landmarks(
    id                    PK,
    zone_id               FK,
    name                  TEXT,
    kind                  TEXT,             -- settlement | dungeon_entrance | shop | temple | ruin | natural_feature
    description           TEXT,
    position_note         TEXT?,            -- free-text: 'at the north end', 'atop the hill'
    hidden                BOOL,             -- secret landmarks start hidden; revealed by discovery events
    corresponds_to_zone_id FK?,             -- if entering this landmark puts you in a different zone
    created_at_event_id   FK?
)

character_zone_knowledge(
    character_id          FK,
    zone_id               FK,
    level                 TEXT,             -- rumored | known | visited | mapped
    first_event_id        FK?,              -- event that granted this knowledge
    last_visit_at_hour    INTEGER?,
    PRIMARY KEY (character_id, zone_id)
)

character_landmark_knowledge(
    character_id          FK,
    landmark_id           FK,
    level                 TEXT,             -- rumored | known | visited
    first_event_id        FK?,
    last_visit_at_hour    INTEGER?,
    PRIMARY KEY (character_id, landmark_id)
)
```

## Key decisions

### Graph, not hex grid

The world is zones-as-nodes and connections-as-directed-edges. Hex grids were considered and deferred because:

- Graphs represent narrative geography naturally. "Greenhollow Forest" is a single zone; it doesn't need tile-by-tile authoring.
- The MVP consumer is an LLM DM agent that narrates space — it doesn't need tactical coordinates.
- Hex grids would triple the authoring load (generate terrain for every tile) for minimal gameplay gain at this stage.

**Forward compatibility with hexes is maintained** by the `direction_from` column on connections. When adding a hex layer later, zones gain hex coordinates and connections imply cell adjacency; the graph structure remains valid as a coarser "region" overlay. Nothing in the current design blocks that.

### Directed edges, two rows per bidirectional connection

Most connections are bidirectional, so they show up as two rows: A→B and B→A. This costs a bit of storage and makes `one_way` asymmetries trivially representable. "Where can I go from here?" is a single indexed query on `from_zone_id` with no `OR` / `UNION` gymnastics.

### `direction_from` is the minimum spatial data we need

One column unblocks world-map rendering today (place zones in 2D by walking directions from the starting zone) and maps cleanly onto hex neighbours later (8 horizontal directions + up/down align with hex conventions). Without it, the graph is purely topological and later spatial layouts require guessing.

### Nested zones for complex structures, flat for simple ones

A **dungeon** is a zone with `kind='dungeon'`. Each of its floors is a child zone with `kind='dungeon_floor'` and `parent_zone_id = dungeon`. Each of a floor's rooms is a child with `kind='dungeon_room'` and `parent_zone_id = floor`. Stairs between floors are `zone_connections` with `direction_from='up'` or `'down'`.

A **single-space encounter** — a forest clearing ambush, a roadside meeting — stays flat: one zone with `kind='wilderness'`, participants placed without sub-rooms, no children. The nesting is proportional to structural complexity.

### Fog-of-war knowledge is per-character, monotonic upward

Knowledge levels form a strict order: `rumored < known < visited < mapped`. Once a character has a knowledge level, they don't lose it except through an explicit event (magical memory loss, supernatural amnesia) which is a rare, deliberate action.

`mapped` exists only for zones (buying or finding a map of a region); landmarks, being points rather than areas, top out at `visited`.

Per-character rather than per-party because:
- A newly-met companion hasn't visited the places the player has. The companion can be told ("I've been to Ashfield — it's two days north"), which inserts a `rumored` row for them.
- "What does this NPC know about the world?" uses the same queries as "what does the player know."

### Landmarks can reference zones

A settlement is usually both a landmark (visible in its containing wilderness zone) **and** a zone of its own (for when the player is inside it). `landmarks.corresponds_to_zone_id` links them:

- The forest zone contains a landmark `{name: "Ashfield", corresponds_to_zone_id: ashfield_zone.id}`.
- Entering Ashfield via `world.travel(character, ashfield_zone.id)` is normal travel; the landmark is just a visual/narrative anchor for the approach.

## Lazy generation

The world grows outward from the player. It is **not** fully generated at campaign start.

### Two levels of generation

**Stub generation** runs on adjacency discovery. When the player enters a zone, its direct neighbours are ensured to exist as stub rows: `{name, biome, kind, size}` plus a `zone_connection` back. No landmarks, no internal detail, no NPCs. Cheap and fast.

**Full generation** runs on first visit. When a character's `character_zone_knowledge.level` upgrades from `rumored`/`known` → `visited` for the first time, the full generator fires and populates the zone with landmarks, encounter-tag pool, resident NPCs, and pre-history events.

Full generation happens **once per zone** and caches its result in the database; re-entries are free.

### Why lazy — and why this feels "alive"

The full-generation pass runs **with read access to the entire campaign state**: the event log, all NPCs known to exist, active plot threads, the big bad's recent moves, items the player is seeking.

Before placing a new landmark or NPC, the generator asks: *can an existing entity fit here?* An orc the player has fought could turn up here. The big bad's known agents could have reached this region. A lost item from an earlier backstory event could surface in this landmark.

This is the **reconciliation pass** (see [NPC generation](npcs.md#reconciliation)) — and it's the reason lazily-generated zones feel like part of a single coherent world rather than procedurally-stitched bits. Every late-discovered zone can be woven into the established narrative because the generator sees the full narrative when it runs.

## Dungeons

Dungeon generation is template-driven procedural. A dungeon template lives in content:

```yaml
# content/world/dungeon_templates.yaml
abandoned_wizard_tower:
  floor_count: "1d4+2"              # 3-6 floors
  rooms_per_floor: "2d4+2"          # 4-10 rooms
  connection_density: 0.5           # probability of a non-tree edge between two rooms on the same floor
  encounter_tags: [undead, arcane_guardians, magical_traps]
  landmark_kinds: [library, laboratory, summoning_circle, sanctum]
  boss_depth: last_floor
  treasure_depth: middle_or_last
```

Procedure:

1. Create parent zone, `kind='dungeon'`, biome from template.
2. Roll `floor_count`; create that many child zones (parent=dungeon), `kind='dungeon_floor'`.
3. For each floor, roll `rooms_per_floor`; create child zones (parent=floor), `kind='dungeon_room'`.
4. Connect rooms on each floor: start with a spanning tree (ensures connectivity), then add extra edges up to the density parameter.
5. Connect floors via stairs: pick one room per floor as stair-up, another as stair-down; add connections with `direction_from='up'`/`'down'`.
6. Assign encounters from the tag pool, weighted by depth (harder encounters deeper).
7. Place landmarks per template (boss room on last floor, treasure room biased toward middle/last).

Bounded, deterministic given seeds, and entirely content-controlled.

## Travel

`world.travel(character_id, to_zone_id)`:

1. Validate: `zone_connections` contains an edge from `character.current_zone_id` to `to_zone_id`.
2. Advance `campaign_hour` by the edge's `travel_time_hours`; tick hour-based effect/condition expiries.
3. Roll for a random encounter using the destination biome's `encounter_tags` combined with the connection's `hazard_tag`.
4. Update `characters.current_zone_id`.
5. Upsert `character_zone_knowledge` to `visited` if the level was lower.
6. Trigger stub-generation for any of the destination's neighbours that don't yet exist.
7. Trigger full-generation of the destination zone if this is the first visit.
8. Emit events: `location.move`, optionally `encounter.rolled`, generation events for any new zones/landmarks/NPCs created.

Travel is where world time actually advances in coherent chunks — the clock ticks forward, maybe an encounter interrupts, visibility updates.

## World map

`world.map(character_id)` returns a fog-filtered graph the agent can render (ASCII, text, or piped to a viz client):

- **Zones** the asking character has at least `rumored` knowledge of, each with its current knowledge level.
- **Connections** between zones where both endpoints are at least `known` (or one `visited` + one `rumored` — the edge is known, the destination hazy).
- **Landmarks** the asking character has at least `rumored` knowledge of.
- **Computed 2D positions** for each zone, derived from `direction_from` on connections with the character's current zone as origin `(0, 0)`.

The output is structured JSON; rendering is the agent's job. For MVP, an ASCII-art rendering companion tool can be added, but the MCP guarantees only the structured graph.

## Tools

| Tool                                                        | Effect                                                                 |
|-------------------------------------------------------------|------------------------------------------------------------------------|
| `world.travel(character_id, to_zone_id)`                    | Move + advance time + roll encounter + update fog. Described above.    |
| `world.map(character_id)`                                   | Fog-filtered structured graph with 2D positions.                       |
| `world.describe_zone(character_id, zone_id?)`               | Prose + landmarks + NPCs for a zone the character has visited.         |
| `world.list_connections(character_id)`                      | "Where can I go from here?" — filtered by knowledge level.             |
| `world.reveal_landmark(character_id, landmark_id)`          | Bump the character's landmark knowledge level.                         |
| `world.learn_of_zone(character_id, zone_id, level)`         | Grant rumored/known knowledge (NPC tells the player about a place).    |
| `world.generate_zone(from_zone_id, direction, biome_hint?)` | Admin/testing; normally triggered automatically by travel.             |
