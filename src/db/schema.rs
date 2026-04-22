//! Campaign database schema — every table from `docs/`, created in one migration at startup.
//!
//! See the per-area docs for the field-level rationale:
//!   - `docs/history-log.md`   — events, event_participants, event_items, two-tier time
//!   - `docs/characters.md`    — characters, parties, character_resources, death flow
//!   - `docs/checks.md`        — character_proficiencies, effects, character_conditions
//!   - `docs/items.md`         — items, item_enchantments, location-mutex CHECK
//!   - `docs/world.md`         — zones, zone_connections, landmarks, knowledge tables
//!   - `docs/encounters.md`    — encounters, encounter_participants, combat fields inline
//!   - `docs/campaign-setup.md`— campaign_state (singleton), campaign_setup_answers
//!
//! Future phases will extend this module with migration-history tracking when the schema
//! starts evolving across released versions. Phase 2 is a greenfield create-all-tables pass.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Every table that Phase 2 creates. Kept here so tests can assert coverage without
/// duplicating the list. Not yet consumed outside tests — later phases will reuse it for
/// introspection / migration-history tooling.
#[allow(dead_code)]
pub const EXPECTED_TABLES: &[&str] = &[
    // Entities
    "parties",
    "characters",
    "character_proficiencies",
    "character_resources",
    "character_conditions",
    "effects",
    // Items
    "items",
    "item_enchantments",
    // World
    "zones",
    "zone_connections",
    "landmarks",
    "character_zone_knowledge",
    "character_landmark_knowledge",
    // Encounters + combat
    "encounters",
    "encounter_participants",
    // History
    "events",
    "event_participants",
    "event_items",
    // Campaign lifecycle
    "campaign_state",
    "campaign_setup_answers",
];

/// Create every table and index in a single transaction. Idempotent: re-running on a DB that
/// already has the schema is a no-op because every CREATE uses IF NOT EXISTS.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction().context("begin migration tx")?;
    tx.execute_batch(DDL).context("apply schema DDL")?;
    tx.commit().context("commit migration tx")?;
    Ok(())
}

/// All CREATE TABLE + CREATE INDEX statements. Kept as one string so the migration is one
/// `execute_batch` call in one transaction — atomic schema creation.
const DDL: &str = r#"
-- ─────────────────────────────────────────────────────────────────────────────
-- Parties: pure grouping, no stats or shared inventory. docs/characters.md
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS parties (
    id           INTEGER PRIMARY KEY,
    name         TEXT,
    created_at   INTEGER NOT NULL    -- campaign_hour
);

-- ─────────────────────────────────────────────────────────────────────────────
-- Characters: player, companions, pets, friendly NPCs, enemies — one table.
-- Ability scores are BASE values; effective values compose with `effects` at read.
-- docs/characters.md + docs/checks.md
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS characters (
    id                      INTEGER PRIMARY KEY,
    name                    TEXT NOT NULL,
    role                    TEXT NOT NULL CHECK (role IN
                              ('player', 'companion', 'friendly', 'enemy', 'neutral')),
    party_id                INTEGER REFERENCES parties(id),

    -- Six ability scores (base)
    str_score               INTEGER NOT NULL,
    dex_score               INTEGER NOT NULL,
    con_score               INTEGER NOT NULL,
    int_score               INTEGER NOT NULL,
    wis_score               INTEGER NOT NULL,
    cha_score               INTEGER NOT NULL,

    -- Combat numbers
    hp_current              INTEGER NOT NULL,
    hp_max                  INTEGER NOT NULL,
    hp_temp                 INTEGER NOT NULL DEFAULT 0,
    armor_class             INTEGER NOT NULL,
    speed_ft                INTEGER NOT NULL DEFAULT 30,
    initiative_bonus        INTEGER NOT NULL DEFAULT 0,

    -- Progression (denormalised from event log for read latency)
    level                   INTEGER NOT NULL DEFAULT 1,
    xp_total                INTEGER NOT NULL DEFAULT 0,
    proficiency_bonus       INTEGER NOT NULL DEFAULT 2,

    -- Physical
    size                    TEXT NOT NULL DEFAULT 'medium'
                              CHECK (size IN ('tiny','small','medium','large','huge','gargantuan')),

    -- Narrative labels (LLM-interpreted)
    species                 TEXT,
    class_or_archetype      TEXT,
    ideology                TEXT,
    backstory               TEXT,
    plans                   TEXT,

    -- Party mechanics
    loyalty                 INTEGER NOT NULL DEFAULT 50
                              CHECK (loyalty BETWEEN 0 AND 100),

    -- Lifecycle
    status                  TEXT NOT NULL DEFAULT 'alive'
                              CHECK (status IN ('alive','unconscious','dead','missing')),
    current_zone_id         INTEGER,   -- FK resolved after zones created; SQLite resolves lazily
    death_save_successes    INTEGER NOT NULL DEFAULT 0
                              CHECK (death_save_successes BETWEEN 0 AND 3),
    death_save_failures     INTEGER NOT NULL DEFAULT 0
                              CHECK (death_save_failures BETWEEN 0 AND 3),

    created_at              INTEGER NOT NULL,    -- campaign_hour
    updated_at              INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_characters_role          ON characters(role);
CREATE INDEX IF NOT EXISTS idx_characters_party         ON characters(party_id);
CREATE INDEX IF NOT EXISTS idx_characters_current_zone  ON characters(current_zone_id);
CREATE INDEX IF NOT EXISTS idx_characters_status        ON characters(status);

-- ─────────────────────────────────────────────────────────────────────────────
-- Unified proficiencies: skills, saves (name='save:str'), weapons, tools,
-- custom/pet growth-skills all share this shape. docs/checks.md
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS character_proficiencies (
    character_id   INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    proficient     INTEGER NOT NULL DEFAULT 0 CHECK (proficient IN (0,1)),
    expertise      INTEGER NOT NULL DEFAULT 0 CHECK (expertise IN (0,1)),
    ranks          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (character_id, name)
);

-- ─────────────────────────────────────────────────────────────────────────────
-- Limited-use resources: spell slots, mana, ki, rage, hit dice, etc.
-- Generic — the agent interprets `name`. docs/characters.md §Resources
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS character_resources (
    character_id   INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    name           TEXT NOT NULL,     -- 'slot:1' .. 'slot:9', 'hit_die', 'mana', 'ki', ...
    current        INTEGER NOT NULL,
    max            INTEGER NOT NULL,
    recharge       TEXT NOT NULL CHECK (recharge IN
                     ('short_rest','long_rest','dawn','never','manual')),
    PRIMARY KEY (character_id, name)
);

-- ─────────────────────────────────────────────────────────────────────────────
-- Conditions: named states with mechanical riders (defined in content, not here).
-- docs/checks.md §Conditions
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS character_conditions (
    id                    INTEGER PRIMARY KEY,
    character_id          INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    condition             TEXT NOT NULL,    -- 'blinded', 'poisoned', 'exhaustion', etc.
    severity              INTEGER NOT NULL DEFAULT 1,
    source_event_id       INTEGER,
    expires_at_hour       INTEGER,
    expires_after_rounds  INTEGER,
    remove_on_save        TEXT,             -- e.g. 'save:con:dc15'
    active                INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1))
);
CREATE INDEX IF NOT EXISTS idx_conditions_character_active
    ON character_conditions(character_id, active);

-- ─────────────────────────────────────────────────────────────────────────────
-- Effects: temporary numerical modifiers. Never mutate base stats.
-- docs/checks.md §Effects
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS effects (
    id                     INTEGER PRIMARY KEY,
    target_character_id    INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    source                 TEXT NOT NULL,
    target_kind            TEXT NOT NULL CHECK (target_kind IN
                             ('ability','ac','speed','hp_max','attack','damage','skill','save','misc')),
    target_key             TEXT NOT NULL,
    modifier               INTEGER NOT NULL DEFAULT 0,
    dice_expr              TEXT,
    start_event_id         INTEGER NOT NULL,
    expires_at_hour        INTEGER,
    expires_after_rounds   INTEGER,
    expires_on_dispel      INTEGER NOT NULL DEFAULT 0 CHECK (expires_on_dispel IN (0,1)),
    active                 INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0,1))
);
CREATE INDEX IF NOT EXISTS idx_effects_target_active
    ON effects(target_character_id, active);

-- ─────────────────────────────────────────────────────────────────────────────
-- Items: one table, location-mutex enforced by CHECK. docs/items.md
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS items (
    id                    INTEGER PRIMARY KEY,
    base_kind             TEXT NOT NULL,
    name                  TEXT,
    material              TEXT,
    material_tier         INTEGER,
    quality               TEXT,
    quantity              INTEGER NOT NULL DEFAULT 1 CHECK (quantity > 0),
    charges               INTEGER,
    charges_max           INTEGER,

    holder_character_id   INTEGER REFERENCES characters(id) ON DELETE SET NULL,
    container_item_id     INTEGER REFERENCES items(id)      ON DELETE SET NULL,
    zone_location_id      INTEGER,     -- zones(id) — FK added via trigger-free CHECK

    equipped_slot         TEXT,

    created_at_event_id   INTEGER,
    updated_at            INTEGER NOT NULL,

    -- Exactly one of the three location FKs must be non-null.
    CHECK (
        ((holder_character_id IS NOT NULL)
       + (container_item_id   IS NOT NULL)
       + (zone_location_id    IS NOT NULL)) = 1
    )
);
CREATE INDEX IF NOT EXISTS idx_items_holder        ON items(holder_character_id);
CREATE INDEX IF NOT EXISTS idx_items_container     ON items(container_item_id);
CREATE INDEX IF NOT EXISTS idx_items_zone          ON items(zone_location_id);
CREATE INDEX IF NOT EXISTS idx_items_base_kind     ON items(base_kind);

CREATE TABLE IF NOT EXISTS item_enchantments (
    id                 INTEGER PRIMARY KEY,
    item_id            INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    enchantment_kind   TEXT NOT NULL,
    tier               INTEGER,
    charges            INTEGER,
    notes              TEXT
);
CREATE INDEX IF NOT EXISTS idx_enchantments_item ON item_enchantments(item_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- Zones, connections, landmarks. Graph-of-zones model. docs/world.md
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS zones (
    id                     INTEGER PRIMARY KEY,
    name                   TEXT NOT NULL,
    biome                  TEXT NOT NULL,
    kind                   TEXT NOT NULL CHECK (kind IN
                             ('wilderness','settlement','dungeon','dungeon_floor',
                              'dungeon_room','road','liminal')),
    size                   TEXT NOT NULL CHECK (size IN
                             ('tiny','small','medium','large','vast')),
    parent_zone_id         INTEGER REFERENCES zones(id) ON DELETE CASCADE,
    description            TEXT,
    encounter_tags         TEXT NOT NULL DEFAULT '[]',   -- JSON array
    created_at_event_id    INTEGER,
    notes                  TEXT
);
CREATE INDEX IF NOT EXISTS idx_zones_parent ON zones(parent_zone_id);
CREATE INDEX IF NOT EXISTS idx_zones_biome  ON zones(biome);

CREATE TABLE IF NOT EXISTS zone_connections (
    from_zone_id           INTEGER NOT NULL REFERENCES zones(id) ON DELETE CASCADE,
    to_zone_id             INTEGER NOT NULL REFERENCES zones(id) ON DELETE CASCADE,
    travel_time_hours      INTEGER NOT NULL CHECK (travel_time_hours >= 0),
    travel_mode            TEXT NOT NULL CHECK (travel_mode IN
                             ('road','wilderness','portal','passage','climb')),
    hazard_tag             TEXT,
    one_way                INTEGER NOT NULL DEFAULT 0 CHECK (one_way IN (0,1)),
    direction_from         TEXT NOT NULL CHECK (direction_from IN
                             ('n','ne','e','se','s','sw','w','nw','up','down')),
    PRIMARY KEY (from_zone_id, to_zone_id)
);

CREATE TABLE IF NOT EXISTS landmarks (
    id                       INTEGER PRIMARY KEY,
    zone_id                  INTEGER NOT NULL REFERENCES zones(id) ON DELETE CASCADE,
    name                     TEXT NOT NULL,
    kind                     TEXT NOT NULL CHECK (kind IN
                               ('settlement','dungeon_entrance','shop','temple',
                                'ruin','natural_feature')),
    description              TEXT,
    position_note            TEXT,
    hidden                   INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0,1)),
    corresponds_to_zone_id   INTEGER REFERENCES zones(id) ON DELETE SET NULL,
    created_at_event_id      INTEGER
);
CREATE INDEX IF NOT EXISTS idx_landmarks_zone ON landmarks(zone_id);

-- Per-character fog of war. Monotonic upward — `level` only increases in normal play.
CREATE TABLE IF NOT EXISTS character_zone_knowledge (
    character_id         INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    zone_id              INTEGER NOT NULL REFERENCES zones(id)      ON DELETE CASCADE,
    level                TEXT NOT NULL CHECK (level IN ('rumored','known','visited','mapped')),
    first_event_id       INTEGER,
    last_visit_at_hour   INTEGER,
    PRIMARY KEY (character_id, zone_id)
);

CREATE TABLE IF NOT EXISTS character_landmark_knowledge (
    character_id         INTEGER NOT NULL REFERENCES characters(id)  ON DELETE CASCADE,
    landmark_id          INTEGER NOT NULL REFERENCES landmarks(id)   ON DELETE CASCADE,
    level                TEXT NOT NULL CHECK (level IN ('rumored','known','visited')),
    first_event_id       INTEGER,
    last_visit_at_hour   INTEGER,
    PRIMARY KEY (character_id, landmark_id)
);

-- ─────────────────────────────────────────────────────────────────────────────
-- Encounters: combat is a mode of an encounter, not a separate table.
-- docs/encounters.md
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS encounters (
    id                         INTEGER PRIMARY KEY,
    zone_id                    INTEGER REFERENCES zones(id) ON DELETE SET NULL,
    name                       TEXT,
    goal                       TEXT,
    estimated_duration_hours   INTEGER NOT NULL DEFAULT 0,
    xp_budget                  INTEGER NOT NULL DEFAULT 0,
    status                     TEXT NOT NULL DEFAULT 'active'
                                 CHECK (status IN
                                   ('active','goal_completed','abandoned','failed')),

    -- Combat state inline; NULL outside combat.
    in_combat                  INTEGER NOT NULL DEFAULT 0 CHECK (in_combat IN (0,1)),
    current_round              INTEGER,
    turn_index                 INTEGER,

    started_at_hour            INTEGER NOT NULL,
    ended_at_hour              INTEGER
);
CREATE INDEX IF NOT EXISTS idx_encounters_zone     ON encounters(zone_id);
CREATE INDEX IF NOT EXISTS idx_encounters_status   ON encounters(status);
CREATE INDEX IF NOT EXISTS idx_encounters_combat   ON encounters(in_combat) WHERE in_combat=1;

CREATE TABLE IF NOT EXISTS encounter_participants (
    encounter_id             INTEGER NOT NULL REFERENCES encounters(id) ON DELETE CASCADE,
    character_id             INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    side                     TEXT NOT NULL CHECK (side IN
                               ('player_side','hostile','neutral','ally')),
    initiative               INTEGER,
    has_acted_this_round     INTEGER NOT NULL DEFAULT 0 CHECK (has_acted_this_round IN (0,1)),
    PRIMARY KEY (encounter_id, character_id)
);

-- ─────────────────────────────────────────────────────────────────────────────
-- History / event log. Polymorphic; payload is JSON; participants + items
-- junctions are the indexed path for "events involving X" queries.
-- docs/history-log.md — append-only.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS events (
    id              INTEGER PRIMARY KEY,
    kind            TEXT NOT NULL,        -- dotted taxonomy: 'combat.hit', 'xp.goal', ...
    campaign_hour   INTEGER NOT NULL,     -- may be negative (pre-campaign backstory)
    combat_round    INTEGER,              -- set only for events inside combat
    zone_id         INTEGER REFERENCES zones(id)      ON DELETE SET NULL,
    encounter_id    INTEGER REFERENCES encounters(id) ON DELETE SET NULL,
    parent_id       INTEGER REFERENCES events(id)     ON DELETE SET NULL,
    summary         TEXT NOT NULL,
    payload         TEXT NOT NULL DEFAULT '{}'    -- JSON
);
CREATE INDEX IF NOT EXISTS idx_events_zone_time     ON events(zone_id, campaign_hour DESC);
CREATE INDEX IF NOT EXISTS idx_events_encounter     ON events(encounter_id);
CREATE INDEX IF NOT EXISTS idx_events_parent        ON events(parent_id);
CREATE INDEX IF NOT EXISTS idx_events_kind_time     ON events(kind, campaign_hour DESC);

CREATE TABLE IF NOT EXISTS event_participants (
    event_id       INTEGER NOT NULL REFERENCES events(id)     ON DELETE CASCADE,
    character_id   INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    role           TEXT NOT NULL CHECK (role IN
                     ('actor','target','witness','beneficiary')),
    PRIMARY KEY (event_id, character_id, role)
);
-- Hottest query path: "events involving character X".
CREATE INDEX IF NOT EXISTS idx_event_participants_char
    ON event_participants(character_id, event_id);

CREATE TABLE IF NOT EXISTS event_items (
    event_id   INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    item_id    INTEGER NOT NULL REFERENCES items(id)  ON DELETE CASCADE,
    role       TEXT NOT NULL,   -- 'stolen' | 'given' | 'destroyed' | 'actor' | etc.
    PRIMARY KEY (event_id, item_id, role)
);
CREATE INDEX IF NOT EXISTS idx_event_items_item ON event_items(item_id, event_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- Campaign lifecycle (singleton + setup-phase answers).
-- docs/campaign-setup.md
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS campaign_state (
    id                     INTEGER PRIMARY KEY CHECK (id = 1),   -- singleton
    phase                  TEXT NOT NULL CHECK (phase IN ('setup','running')),
    started_at             INTEGER,      -- real-time epoch ms when mark_ready fired
    player_character_id    INTEGER REFERENCES characters(id) ON DELETE SET NULL
);

-- Seed the singleton row in the 'setup' phase if missing. Insert OR IGNORE makes the
-- migration idempotent on re-open.
INSERT OR IGNORE INTO campaign_state (id, phase) VALUES (1, 'setup');

CREATE TABLE IF NOT EXISTS campaign_setup_answers (
    question_id   TEXT PRIMARY KEY,
    answer        TEXT NOT NULL,   -- JSON; array for multi-select, string/scalar otherwise
    answered_at   INTEGER NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .expect("enable foreign keys");
        conn
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    }

    #[test]
    fn migrate_creates_every_expected_table() {
        let mut conn = in_memory();
        migrate(&mut conn).expect("first migrate");
        let tables = table_names(&conn);
        for expected in EXPECTED_TABLES {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}; got {tables:?}"
            );
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut conn = in_memory();
        migrate(&mut conn).expect("first migrate");
        migrate(&mut conn).expect("second migrate should be a no-op");
        let tables = table_names(&conn);
        // Same table set both times.
        for expected in EXPECTED_TABLES {
            assert!(tables.iter().any(|t| t == expected));
        }
    }

    #[test]
    fn campaign_state_singleton_is_seeded_in_setup_phase() {
        let mut conn = in_memory();
        migrate(&mut conn).expect("migrate");
        let phase: String = conn
            .query_row("SELECT phase FROM campaign_state WHERE id = 1", [], |r| {
                r.get(0)
            })
            .expect("singleton row exists");
        assert_eq!(phase, "setup");
    }

    #[test]
    fn campaign_state_refuses_second_row() {
        let mut conn = in_memory();
        migrate(&mut conn).expect("migrate");
        let err = conn
            .execute(
                "INSERT INTO campaign_state (id, phase) VALUES (2, 'running')",
                [],
            )
            .expect_err("second row should violate the CHECK (id = 1)");
        assert!(
            format!("{err:#}").contains("CHECK"),
            "expected CHECK violation, got {err}"
        );
    }

    #[test]
    fn item_location_mutex_is_enforced() {
        // Create a character so the holder FK resolves.
        let mut conn = in_memory();
        migrate(&mut conn).expect("migrate");
        conn.execute(
            "INSERT INTO characters (
                name, role,
                str_score, dex_score, con_score, int_score, wis_score, cha_score,
                hp_current, hp_max, armor_class,
                created_at, updated_at
            ) VALUES (?1, 'player', 10,10,10,10,10,10, 10, 10, 10, 0, 0)",
            ["Kira"],
        )
        .expect("insert character");

        // No location → rejected (CHECK = 1).
        let err = conn
            .execute(
                "INSERT INTO items (base_kind, quantity, updated_at) VALUES ('gold', 1, 0)",
                [],
            )
            .expect_err("item with no location should be rejected");
        assert!(format!("{err:#}").contains("CHECK"));

        // Two locations → rejected.
        let err = conn
            .execute(
                "INSERT INTO items (base_kind, quantity, holder_character_id, zone_location_id, updated_at)
                 VALUES ('gold', 1, 1, 999, 0)",
                [],
            )
            .expect_err("item with two locations should be rejected");
        assert!(format!("{err:#}").contains("CHECK"));

        // Exactly one → accepted.
        conn.execute(
            "INSERT INTO items (base_kind, quantity, holder_character_id, updated_at)
             VALUES ('gold', 5, 1, 0)",
            [],
        )
        .expect("holder-only item should be accepted");
    }
}
