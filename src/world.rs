//! Travel + map + zone description.
//!
//! Phase 7 surface (per Roadmap):
//!
//! - [`travel`] — moves a character along an existing zone connection. Advances the
//!   campaign-hour clock by the connection's `travel_time_hours`, updates the character's
//!   `current_zone_id`, upserts `character_zone_knowledge` for the destination to
//!   `visited`, ensures stub neighbours exist on first contact, and on first visit runs
//!   the full-generation pass (Phase 7 minimum: a couple of landmarks).
//! - [`map`] — fog-filtered graph rooted at the character's current zone. Walks
//!   `zone_connections` BFS from the origin, assigning 2D positions from `direction_from`.
//! - [`describe_zone`] — prose / structured detail for a single zone the character knows.
//!
//! See `docs/world.md` for the design.

use std::collections::{BTreeMap, VecDeque};

use anyhow::{bail, Context, Result};
use rand::RngExt;
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};

use crate::events::{self, EventSpec, Participant};

// ── Tool params / results ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TravelParams {
    pub character_id: i64,
    pub to_zone_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TravelResult {
    pub character_id: i64,
    pub from_zone_id: i64,
    pub to_zone_id: i64,
    pub travel_time_hours: i32,
    pub campaign_hour_before: i64,
    pub campaign_hour_after: i64,
    pub knowledge_level: String,
    pub stubs_generated: Vec<i64>,
    pub landmarks_generated: Vec<i64>,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MapParams {
    pub character_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapResult {
    pub origin_zone_id: i64,
    pub zones: Vec<MapZone>,
    pub connections: Vec<MapConnection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapZone {
    pub id: i64,
    pub name: String,
    pub biome: String,
    pub kind: String,
    pub knowledge_level: String,
    /// 2D grid coordinates derived from `direction_from` walking BFS from the origin
    /// (which sits at (0, 0)). Diagonals advance both axes by one.
    pub x: i32,
    pub y: i32,
    pub landmarks: Vec<MapLandmark>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapLandmark {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub knowledge_level: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapConnection {
    pub from_zone_id: i64,
    pub to_zone_id: i64,
    pub direction_from: String,
    pub travel_time_hours: i32,
    pub one_way: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DescribeZoneParams {
    pub character_id: i64,
    pub zone_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DescribeZoneResult {
    pub zone_id: i64,
    pub name: String,
    pub biome: String,
    pub kind: String,
    pub size: String,
    pub description: Option<String>,
    pub knowledge_level: String,
    pub landmarks: Vec<MapLandmark>,
    pub connections: Vec<MapConnection>,
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Move a character along an existing zone connection. Returns the structured travel
/// outcome. All writes are committed inside a single transaction.
pub fn travel(conn: &mut Connection, p: TravelParams) -> Result<TravelResult> {
    // Load preconditions outside the transaction (read-only).
    let from_zone_id = read_current_zone(conn, p.character_id)?.ok_or_else(|| {
        anyhow::anyhow!("character {} has no current_zone_id set", p.character_id)
    })?;
    if from_zone_id == p.to_zone_id {
        bail!(
            "character {} is already in zone {}",
            p.character_id,
            p.to_zone_id
        );
    }

    let conn_row: Option<(i32, String)> = conn
        .query_row(
            "SELECT travel_time_hours, direction_from FROM zone_connections
             WHERE from_zone_id = ?1 AND to_zone_id = ?2",
            params![from_zone_id, p.to_zone_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional_ok()?;
    let (travel_time_hours, _direction_from) = conn_row.ok_or_else(|| {
        anyhow::anyhow!(
            "no zone_connection from zone {} to zone {} exists; can't travel",
            from_zone_id,
            p.to_zone_id
        )
    })?;

    let hour_before = current_campaign_hour(conn)?;
    let hour_after = hour_before + i64::from(travel_time_hours);

    // Whether the destination needs full-generation: this is the character's first visit.
    let dest_known_before = read_zone_knowledge(conn, p.character_id, p.to_zone_id)?;
    let first_visit = !matches!(
        dest_known_before.as_deref(),
        Some("visited") | Some("mapped")
    );

    // Run all writes in a single transaction. Stub-generation of neighbours and
    // first-visit landmark generation produce inserts that must commit together with the
    // location.move event.
    let tx = conn.transaction().context("begin travel tx")?;

    // 1. character.current_zone_id update.
    tx.execute(
        "UPDATE characters SET current_zone_id = ?1, updated_at = ?2 WHERE id = ?3",
        params![p.to_zone_id, hour_after, p.character_id],
    )
    .context("update character.current_zone_id")?;

    // 2. character_zone_knowledge upsert to 'visited' for the destination.
    upsert_knowledge_tx(
        &tx,
        p.character_id,
        p.to_zone_id,
        KnowledgeLevel::Visited,
        hour_after,
    )?;

    // 2b. Defensive: also bump the origin to 'visited'. The character was just there,
    // so they know it. Normally character.create has already seeded this; this catches
    // the edge case where it wasn't (e.g. legacy data, non-API insertion).
    upsert_knowledge_tx(
        &tx,
        p.character_id,
        from_zone_id,
        KnowledgeLevel::Visited,
        hour_before,
    )?;

    // 3. Stub-gen any missing neighbours of the destination zone.
    let stubs_generated = ensure_stub_neighbours_tx(&tx, p.to_zone_id)?;

    // 3b. Mark every neighbour of the destination as at least 'rumored' so the map can
    // show the player their next-move options. Includes any stubs just generated above.
    // upsert_knowledge_tx is monotonic — if the player already knew a neighbour better
    // than rumored, the existing level is preserved.
    let dest_neighbours: Vec<i64> = read_outgoing_edges(&tx, p.to_zone_id)?
        .into_iter()
        .map(|e| e.to_zone_id)
        .collect();
    for nzid in dest_neighbours {
        upsert_knowledge_tx(
            &tx,
            p.character_id,
            nzid,
            KnowledgeLevel::Rumored,
            hour_after,
        )?;
    }

    // 4. First-visit landmark generation.
    let landmarks_generated = if first_visit {
        full_generate_landmarks_tx(&tx, p.to_zone_id)?
    } else {
        Vec::new()
    };

    // 5. location.move event.
    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "location.move",
            campaign_hour: hour_after,
            combat_round: None,
            zone_id: Some(p.to_zone_id),
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={cid} travelled from zone {from} to zone {to} ({hours}h, hour now {hour})",
                cid = p.character_id,
                from = from_zone_id,
                to = p.to_zone_id,
                hours = travel_time_hours,
                hour = hour_after,
            ),
            payload: serde_json::json!({
                "from_zone_id": from_zone_id,
                "to_zone_id": p.to_zone_id,
                "travel_time_hours": travel_time_hours,
                "hour_before": hour_before,
                "hour_after": hour_after,
                "first_visit": first_visit,
                "stubs_generated": stubs_generated,
                "landmarks_generated": landmarks_generated,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "actor",
            }],
            items: &[],
        },
    )?;

    tx.commit().context("commit travel tx")?;

    Ok(TravelResult {
        character_id: p.character_id,
        from_zone_id,
        to_zone_id: p.to_zone_id,
        travel_time_hours,
        campaign_hour_before: hour_before,
        campaign_hour_after: hour_after,
        knowledge_level: "visited".to_string(),
        stubs_generated,
        landmarks_generated,
        event_id: emitted.event_id,
    })
}

/// Build the fog-filtered map rooted at the character's current zone.
pub fn map(conn: &Connection, p: MapParams) -> Result<MapResult> {
    let origin_zone_id = read_current_zone(conn, p.character_id)?.ok_or_else(|| {
        anyhow::anyhow!("character {} has no current_zone_id set", p.character_id)
    })?;

    // BFS from origin assigning grid positions. Limit BFS to known zones (rumored or
    // better) so unknown zones don't leak into the map.
    let known: BTreeMap<i64, String> = read_known_zone_levels(conn, p.character_id)?;
    if !known.contains_key(&origin_zone_id) {
        // Strict knowledge check — symmetric with describe_zone. character.create seeds
        // 'visited' for the starting zone and travel tops it up, so this is unreachable
        // in normal API use. If it does fire, the data is inconsistent and silently
        // returning a map where origin is 'rumored' (the prior fallback) hides the bug.
        bail!(
            "character {} has no knowledge of their current zone {} \
             (data inconsistency — character.create / world.travel should have seeded this)",
            p.character_id,
            origin_zone_id
        );
    }
    let mut positions: BTreeMap<i64, (i32, i32)> = BTreeMap::new();
    positions.insert(origin_zone_id, (0, 0));

    let mut queue: VecDeque<i64> = VecDeque::new();
    queue.push_back(origin_zone_id);

    // Adjacency: for the BFS, fetch all outgoing edges from the seed and from any zone
    // we expand. Cheap for Phase 7-scale graphs.
    while let Some(zid) = queue.pop_front() {
        let (cx, cy) = positions[&zid];
        let edges = read_outgoing_edges(conn, zid)?;
        for e in edges {
            // Only walk into known zones.
            if !known.contains_key(&e.to_zone_id) {
                continue;
            }
            if positions.contains_key(&e.to_zone_id) {
                continue;
            }
            let (dx, dy) = direction_delta(&e.direction_from);
            positions.insert(e.to_zone_id, (cx + dx, cy + dy));
            queue.push_back(e.to_zone_id);
        }
    }

    // Hydrate the zone payloads.
    let mut zones = Vec::new();
    for (zid, (x, y)) in positions {
        // BFS only inserts zones present in `known` (origin is gated by the bail above,
        // expansion checks `known.contains_key` per neighbour), so this lookup never misses.
        let level = known
            .get(&zid)
            .cloned()
            .expect("BFS only walks zones in `known`");
        let row: (String, String, String) = conn.query_row(
            "SELECT name, biome, kind FROM zones WHERE id = ?1",
            [zid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let landmarks = read_landmarks_for_character(conn, p.character_id, zid)?;
        zones.push(MapZone {
            id: zid,
            name: row.0,
            biome: row.1,
            kind: row.2,
            knowledge_level: level,
            x,
            y,
            landmarks,
        });
    }

    // Connections: only between zones both known (rumored at minimum).
    let known_ids: std::collections::HashSet<i64> = zones.iter().map(|z| z.id).collect();
    let mut connections = Vec::new();
    for zid in &known_ids {
        for e in read_outgoing_edges(conn, *zid)? {
            if known_ids.contains(&e.to_zone_id) {
                connections.push(MapConnection {
                    from_zone_id: *zid,
                    to_zone_id: e.to_zone_id,
                    direction_from: e.direction_from,
                    travel_time_hours: e.travel_time_hours,
                    one_way: e.one_way,
                });
            }
        }
    }

    Ok(MapResult {
        origin_zone_id,
        zones,
        connections,
    })
}

/// Detailed readout of one zone, gated by the character's knowledge level.
pub fn describe_zone(conn: &Connection, p: DescribeZoneParams) -> Result<DescribeZoneResult> {
    let level = read_zone_knowledge(conn, p.character_id, p.zone_id)?.ok_or_else(|| {
        anyhow::anyhow!(
            "character {} has no knowledge of zone {} (not even rumored)",
            p.character_id,
            p.zone_id
        )
    })?;

    let (name, biome, kind, size, description): (String, String, String, String, Option<String>) =
        conn.query_row(
            "SELECT name, biome, kind, size, description FROM zones WHERE id = ?1",
            [p.zone_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;

    let landmarks = read_landmarks_for_character(conn, p.character_id, p.zone_id)?;
    let connections: Vec<MapConnection> = read_outgoing_edges(conn, p.zone_id)?
        .into_iter()
        .map(|e| MapConnection {
            from_zone_id: p.zone_id,
            to_zone_id: e.to_zone_id,
            direction_from: e.direction_from,
            travel_time_hours: e.travel_time_hours,
            one_way: e.one_way,
        })
        .collect();

    Ok(DescribeZoneResult {
        zone_id: p.zone_id,
        name,
        biome,
        kind,
        size,
        description,
        knowledge_level: level,
        landmarks,
        connections,
    })
}

// ── Internals ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum KnowledgeLevel {
    Rumored,
    Known,
    Visited,
    Mapped,
}
impl KnowledgeLevel {
    fn as_str(&self) -> &'static str {
        match self {
            KnowledgeLevel::Rumored => "rumored",
            KnowledgeLevel::Known => "known",
            KnowledgeLevel::Visited => "visited",
            KnowledgeLevel::Mapped => "mapped",
        }
    }
    fn rank(&self) -> u8 {
        match self {
            KnowledgeLevel::Rumored => 1,
            KnowledgeLevel::Known => 2,
            KnowledgeLevel::Visited => 3,
            KnowledgeLevel::Mapped => 4,
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "rumored" => Some(KnowledgeLevel::Rumored),
            "known" => Some(KnowledgeLevel::Known),
            "visited" => Some(KnowledgeLevel::Visited),
            "mapped" => Some(KnowledgeLevel::Mapped),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct EdgeRow {
    to_zone_id: i64,
    direction_from: String,
    travel_time_hours: i32,
    one_way: bool,
}

pub(crate) fn current_campaign_hour(conn: &Connection) -> Result<i64> {
    // The current world time is the maximum hour ever stamped on a non-pre-history event.
    // Pre-history (NPC backstory) events have negative campaign_hour so the WHERE clause
    // excludes them.
    let max: Option<i64> = conn
        .query_row(
            "SELECT MAX(campaign_hour) FROM events WHERE campaign_hour >= 0",
            [],
            |row| row.get(0),
        )
        .optional_ok()?
        .flatten();
    Ok(max.unwrap_or(0))
}

fn read_current_zone(conn: &Connection, character_id: i64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT current_zone_id FROM characters WHERE id = ?1",
        [character_id],
        |row| row.get(0),
    )
    .optional_ok()
    .map(|opt| opt.flatten())
}

fn read_zone_knowledge(
    conn: &Connection,
    character_id: i64,
    zone_id: i64,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT level FROM character_zone_knowledge
         WHERE character_id = ?1 AND zone_id = ?2",
        params![character_id, zone_id],
        |row| row.get(0),
    )
    .optional_ok()
}

fn read_known_zone_levels(conn: &Connection, character_id: i64) -> Result<BTreeMap<i64, String>> {
    let mut stmt = conn.prepare(
        "SELECT zone_id, level FROM character_zone_knowledge
         WHERE character_id = ?1",
    )?;
    let rows = stmt.query_map([character_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = BTreeMap::new();
    for r in rows {
        let (id, level) = r?;
        out.insert(id, level);
    }
    Ok(out)
}

fn upsert_knowledge_tx(
    tx: &Transaction<'_>,
    character_id: i64,
    zone_id: i64,
    new_level: KnowledgeLevel,
    visit_hour: i64,
) -> Result<()> {
    // Read existing level — knowledge is monotonic, only upgrade.
    let existing: Option<String> = tx
        .query_row(
            "SELECT level FROM character_zone_knowledge
             WHERE character_id = ?1 AND zone_id = ?2",
            params![character_id, zone_id],
            |row| row.get(0),
        )
        .optional_ok()?;
    let final_level = match existing.as_deref().and_then(KnowledgeLevel::parse) {
        Some(prev) if prev.rank() >= new_level.rank() => prev,
        _ => new_level,
    };
    tx.execute(
        "INSERT INTO character_zone_knowledge (character_id, zone_id, level, last_visit_at_hour)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(character_id, zone_id) DO UPDATE SET
            level = excluded.level,
            last_visit_at_hour = excluded.last_visit_at_hour",
        params![character_id, zone_id, final_level.as_str(), visit_hour],
    )
    .context("upsert character_zone_knowledge")?;
    Ok(())
}

fn read_outgoing_edges(conn: &Connection, from: i64) -> Result<Vec<EdgeRow>> {
    let mut stmt = conn.prepare(
        "SELECT to_zone_id, direction_from, travel_time_hours, one_way
         FROM zone_connections WHERE from_zone_id = ?1",
    )?;
    let rows = stmt.query_map([from], |row| {
        Ok(EdgeRow {
            to_zone_id: row.get(0)?,
            direction_from: row.get(1)?,
            travel_time_hours: row.get(2)?,
            one_way: row.get::<_, i64>(3)? != 0,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn read_landmarks_for_character(
    conn: &Connection,
    character_id: i64,
    zone_id: i64,
) -> Result<Vec<MapLandmark>> {
    let mut stmt = conn.prepare(
        "SELECT l.id, l.name, l.kind, COALESCE(k.level, 'rumored')
         FROM landmarks l
         LEFT JOIN character_landmark_knowledge k
            ON k.landmark_id = l.id AND k.character_id = ?1
         WHERE l.zone_id = ?2 AND l.hidden = 0",
    )?;
    let rows = stmt.query_map(params![character_id, zone_id], |row| {
        Ok(MapLandmark {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            knowledge_level: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// On entry to a zone, ensure each direction available from it has a stub neighbour. The
/// starting zone already has its neighbours from setup::generate_world; this fires only
/// when the player enters a zone whose own neighbours have not yet been generated.
///
/// Returns the list of newly-created neighbour zone IDs (empty when nothing was created).
fn ensure_stub_neighbours_tx(tx: &Transaction<'_>, zone_id: i64) -> Result<Vec<i64>> {
    // A zone with more than one outgoing edge has already been stub-expanded — its
    // neighbours exist beyond just the back-edge to wherever we came from. A zone with
    // exactly one outgoing edge is still a leaf stub: the back-edge is the only
    // connection it carries. Stub-gen targets that case so the player keeps discovering
    // new territory.
    let edge_count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM zone_connections WHERE from_zone_id = ?1",
            [zone_id],
            |row| row.get(0),
        )
        .context("count outgoing edges")?;
    if edge_count > 1 {
        return Ok(Vec::new());
    }

    // Read this zone's biome to seed the stubs with the same biome (Phase 7 minimum).
    let biome: String = tx
        .query_row("SELECT biome FROM zones WHERE id = ?1", [zone_id], |row| {
            row.get(0)
        })
        .context("read source biome for stub-gen")?;

    let mut rng = rand::rng();
    let count = rng.random_range(2u8..=4);
    const DIRECTIONS: &[&str] = &["n", "ne", "e", "se", "s", "sw", "w", "nw"];
    let mut chosen: Vec<&'static str> = DIRECTIONS.to_vec();
    for i in (1..chosen.len()).rev() {
        let j = rng.random_range(0..=i);
        chosen.swap(i, j);
    }

    let mut neighbours = Vec::with_capacity(count as usize);
    for dir in chosen.into_iter().take(count as usize) {
        let neighbour_name = format!("Unexplored {label}", label = direction_label(dir));
        tx.execute(
            "INSERT INTO zones (name, biome, kind, size, encounter_tags)
             VALUES (?1, ?2, 'wilderness', 'small', '[]')",
            params![neighbour_name, biome],
        )?;
        let neighbour_id = tx.last_insert_rowid();
        // Forward.
        tx.execute(
            "INSERT INTO zone_connections
                (from_zone_id, to_zone_id, travel_time_hours, travel_mode, one_way, direction_from)
             VALUES (?1, ?2, 2, 'wilderness', 0, ?3)",
            params![zone_id, neighbour_id, dir],
        )?;
        // Reverse.
        tx.execute(
            "INSERT INTO zone_connections
                (from_zone_id, to_zone_id, travel_time_hours, travel_mode, one_way, direction_from)
             VALUES (?1, ?2, 2, 'wilderness', 0, ?3)",
            params![neighbour_id, zone_id, opposite_direction(dir)],
        )?;
        neighbours.push(neighbour_id);
    }
    Ok(neighbours)
}

/// Phase 7 first-visit full-generation: drop a couple of generic landmarks into the zone.
/// Phase 8 will add NPC placement; later content phases will refine landmark variety from
/// the biome's templates.
fn full_generate_landmarks_tx(tx: &Transaction<'_>, zone_id: i64) -> Result<Vec<i64>> {
    let mut rng = rand::rng();
    let count = rng.random_range(1..=2);
    let candidates = [
        ("A weathered stone marker", "natural_feature"),
        ("A crooked path", "natural_feature"),
        ("A clearing with old fire-pit", "natural_feature"),
        ("Roadside ruin", "ruin"),
    ];
    let mut chosen_idxs: Vec<usize> = (0..candidates.len()).collect();
    for i in (1..chosen_idxs.len()).rev() {
        let j = rng.random_range(0..=i);
        chosen_idxs.swap(i, j);
    }

    let mut ids = Vec::with_capacity(count as usize);
    for idx in chosen_idxs.into_iter().take(count as usize) {
        let (name, kind) = candidates[idx];
        tx.execute(
            "INSERT INTO landmarks (zone_id, name, kind, hidden) VALUES (?1, ?2, ?3, 0)",
            params![zone_id, name, kind],
        )?;
        ids.push(tx.last_insert_rowid());
    }
    Ok(ids)
}

fn direction_delta(d: &str) -> (i32, i32) {
    match d {
        "n" => (0, 1),
        "ne" => (1, 1),
        "e" => (1, 0),
        "se" => (1, -1),
        "s" => (0, -1),
        "sw" => (-1, -1),
        "w" => (-1, 0),
        "nw" => (-1, 1),
        // up/down stack vertically — collapsed to 0 in 2D, agent renders separately.
        _ => (0, 0),
    }
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
    use crate::content::Content;
    use crate::db::schema;
    use crate::setup::{self, AnswerParams as SetupAnswerParams};

    fn fresh() -> (Connection, Content) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        (conn, Content::load(None).unwrap())
    }

    fn bootstrap_world(conn: &mut Connection, content: &Content) -> (i64, Vec<i64>, i64) {
        // Run the Phase 6 setup bootstrap to get a starting zone + neighbours, then
        // create a player character placed in the starting zone.
        setup::answer(
            conn,
            content,
            SetupAnswerParams {
                question_id: "starting_biome".into(),
                answer: serde_json::json!("temperate_forest"),
            },
        )
        .unwrap();
        let gw = setup::generate_world(conn, content).unwrap();
        let player = characters::create(
            conn,
            CreateParams {
                name: "Kira".into(),
                role: "player".into(),
                str_score: 10,
                dex_score: 10,
                con_score: 10,
                int_score: 10,
                wis_score: 10,
                cha_score: 10,
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
                current_zone_id: Some(gw.starting_zone_id),
            },
        )
        .unwrap()
        .character_id;
        // characters::create now seeds 'visited' knowledge of the starting zone as part
        // of the same transaction — no manual upsert needed here.
        (player, gw.neighbour_zone_ids, gw.starting_zone_id)
    }

    #[test]
    fn travel_advances_clock_and_knowledge() {
        let (mut conn, content) = fresh();
        let (player, neighbours, _starting) = bootstrap_world(&mut conn, &content);
        let target = neighbours[0];

        let r = travel(
            &mut conn,
            TravelParams {
                character_id: player,
                to_zone_id: target,
            },
        )
        .expect("travel");

        assert_eq!(r.knowledge_level, "visited");
        assert!(
            r.campaign_hour_after > r.campaign_hour_before,
            "clock should advance: before={} after={}",
            r.campaign_hour_before,
            r.campaign_hour_after
        );

        // Character is now in the target zone.
        let cur: i64 = conn
            .query_row(
                "SELECT current_zone_id FROM characters WHERE id = ?1",
                [player],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cur, target);

        // Knowledge upserted to visited.
        let lvl: String = conn
            .query_row(
                "SELECT level FROM character_zone_knowledge
                 WHERE character_id = ?1 AND zone_id = ?2",
                params![player, target],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lvl, "visited");
    }

    #[test]
    fn travel_rejects_missing_connection() {
        let (mut conn, content) = fresh();
        let (player, _neighbours, _starting) = bootstrap_world(&mut conn, &content);
        // Insert an unconnected island zone.
        conn.execute(
            "INSERT INTO zones (name, biome, kind, size, encounter_tags)
             VALUES ('Island', 'temperate_forest', 'wilderness', 'small', '[]')",
            [],
        )
        .unwrap();
        let island = conn.last_insert_rowid();
        let err = travel(
            &mut conn,
            TravelParams {
                character_id: player,
                to_zone_id: island,
            },
        )
        .expect_err("no connection should reject travel");
        assert!(format!("{err:#}").contains("zone_connection"));
    }

    #[test]
    fn travel_first_visit_generates_stubs_and_landmarks() {
        let (mut conn, content) = fresh();
        let (player, neighbours, _starting) = bootstrap_world(&mut conn, &content);
        let target = neighbours[0];

        let r = travel(
            &mut conn,
            TravelParams {
                character_id: player,
                to_zone_id: target,
            },
        )
        .unwrap();
        // First visit, so stubs + landmarks should be non-empty (target had no outgoing
        // edges of its own before, so stub-gen fires).
        assert!(
            !r.stubs_generated.is_empty(),
            "first visit to a stub neighbour should generate its own neighbours"
        );
        assert!(
            !r.landmarks_generated.is_empty(),
            "first visit should generate at least one landmark"
        );
    }

    #[test]
    fn map_returns_origin_at_zero_zero_with_known_zones() {
        let (mut conn, content) = fresh();
        let (player, neighbours, starting) = bootstrap_world(&mut conn, &content);
        // Make one neighbour visited via travel so the map shows both.
        let target = neighbours[0];
        travel(
            &mut conn,
            TravelParams {
                character_id: player,
                to_zone_id: target,
            },
        )
        .unwrap();
        // After travel, the player is at the target. Travel back to make starting=origin.
        travel(
            &mut conn,
            TravelParams {
                character_id: player,
                to_zone_id: starting,
            },
        )
        .unwrap();

        let m = map(
            &conn,
            MapParams {
                character_id: player,
            },
        )
        .unwrap();
        assert_eq!(m.origin_zone_id, starting);
        // Origin should be at (0, 0).
        let origin = m.zones.iter().find(|z| z.id == starting).unwrap();
        assert_eq!((origin.x, origin.y), (0, 0));
        // Target should be present and offset from origin.
        let visited_target = m.zones.iter().find(|z| z.id == target).unwrap();
        assert_eq!(visited_target.knowledge_level, "visited");
        assert!(
            (visited_target.x, visited_target.y) != (0, 0),
            "neighbour should have non-zero coords"
        );
        // At least one connection between starting and target.
        assert!(
            m.connections
                .iter()
                .any(|c| (c.from_zone_id, c.to_zone_id) == (starting, target)
                    || (c.from_zone_id, c.to_zone_id) == (target, starting)),
            "map should include the starting↔target connection"
        );
    }

    #[test]
    fn describe_zone_requires_some_knowledge() {
        let (mut conn, content) = fresh();
        let (player, _neighbours, _starting) = bootstrap_world(&mut conn, &content);
        // Insert an unconnected unknown zone.
        conn.execute(
            "INSERT INTO zones (name, biome, kind, size, encounter_tags)
             VALUES ('Hidden Vale', 'temperate_forest', 'wilderness', 'small', '[]')",
            [],
        )
        .unwrap();
        let hidden = conn.last_insert_rowid();
        let err = describe_zone(
            &conn,
            DescribeZoneParams {
                character_id: player,
                zone_id: hidden,
            },
        )
        .expect_err("describe_zone should refuse zones the character has no knowledge of");
        assert!(format!("{err:#}").contains("no knowledge"));
    }
}
