//! Inventory: items CRUD + pickup/drop with encumbrance enforcement.
//!
//! Phase 10 surface (per the Roadmap):
//!
//! - [`create`] — insert an item at exactly one location (holder, zone, or container).
//! - [`pickup`] — transfer a zone-located item into a character's inventory. Refuses
//!   the pickup if it would push total carried weight over `overloaded_threshold_pct`;
//!   applies or clears the `encumbered` condition as the carried-weight percentage
//!   crosses `encumbered_threshold_pct`.
//! - [`drop`] — transfer a held item to the character's current zone. Also re-evaluates
//!   encumbrance (crossing back below the threshold clears `encumbered`).
//! - [`transfer`] — generic move between holder/container/zone locations.
//! - [`equip`] / [`unequip`] — set/clear `equipped_slot`.
//! - [`get`] — full per-character inventory readout with effective weights.
//! - [`inspect`] — effective stats of one item.
//!
//! Weight is computed from `content/items/bases/*.yaml` (base weight_lb × quantity);
//! capacity is STR × `encumbrance.capacity_per_str`. See `docs/items.md §Encumbrance`.

use anyhow::{bail, Context, Result};
use rusqlite::OptionalExtension;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::content::{BaseItem, Content};
use crate::events::{self, EventSpec, Participant};

// ── Params / results ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CreateParams {
    pub base_kind: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub material: Option<String>,
    #[serde(default)]
    pub material_tier: Option<i32>,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub quantity: Option<i64>,
    // Exactly one of the three must be set — enforced at the DB layer by the location-mutex
    // CHECK, but we short-circuit here so the error is obvious.
    #[serde(default)]
    pub holder_character_id: Option<i64>,
    #[serde(default)]
    pub zone_location_id: Option<i64>,
    #[serde(default)]
    pub container_item_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateResult {
    pub item_id: i64,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PickupParams {
    pub character_id: i64,
    pub item_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PickupResult {
    pub item_id: i64,
    pub character_id: i64,
    pub carried_weight_lb: f64,
    pub capacity_lb: f64,
    pub percent_of_capacity: i32,
    pub encumbered: bool,
    pub encumbered_condition_id: Option<i64>,
    pub event_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PickupRefused {
    pub error: &'static str,
    pub character_id: i64,
    pub item_id: i64,
    pub would_be_weight_lb: f64,
    pub capacity_lb: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DropParams {
    pub character_id: i64,
    pub item_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DropResult {
    pub item_id: i64,
    pub zone_id: i64,
    pub carried_weight_lb: f64,
    pub capacity_lb: f64,
    pub percent_of_capacity: i32,
    pub encumbered: bool,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EquipParams {
    pub character_id: i64,
    pub item_id: i64,
    pub slot: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UnequipParams {
    pub character_id: i64,
    pub item_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EquipResult {
    pub item_id: i64,
    pub slot: Option<String>,
    pub event_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetParams {
    pub character_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InventoryView {
    pub character_id: i64,
    pub items: Vec<ItemView>,
    pub carried_weight_lb: f64,
    pub capacity_lb: f64,
    pub percent_of_capacity: i32,
    pub encumbered: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemView {
    pub id: i64,
    pub base_kind: String,
    pub name: Option<String>,
    pub material: Option<String>,
    pub material_tier: Option<i32>,
    pub quality: Option<String>,
    pub quantity: i64,
    pub equipped_slot: Option<String>,
    pub effective_weight_lb: f64,
    pub effective_value_gp: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct InspectParams {
    pub item_id: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TransferParams {
    pub item_id: i64,
    #[serde(default)]
    pub to_character_id: Option<i64>,
    #[serde(default)]
    pub to_container_item_id: Option<i64>,
    #[serde(default)]
    pub to_zone_location_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferResult {
    pub item_id: i64,
    pub event_id: i64,
}

// ── Implementation ───────────────────────────────────────────────────────────

pub fn create(conn: &mut Connection, content: &Content, p: CreateParams) -> Result<CreateResult> {
    let base = content
        .item_bases
        .get(&p.base_kind)
        .ok_or_else(|| anyhow::anyhow!("unknown base_kind {:?}", p.base_kind))?;

    let location_count = p.holder_character_id.is_some() as u8
        + p.zone_location_id.is_some() as u8
        + p.container_item_id.is_some() as u8;
    if location_count != 1 {
        bail!(
            "exactly one of holder_character_id, zone_location_id, container_item_id must be set (got {location_count})"
        );
    }

    let quantity = p.quantity.unwrap_or(1);
    if quantity < 1 {
        bail!("quantity must be >= 1 (got {quantity})");
    }
    if quantity > 1 && !base.stackable {
        bail!(
            "base_kind {:?} is not stackable; quantity must be 1",
            p.base_kind
        );
    }

    let now = crate::world::current_campaign_hour(conn)?;

    let tx = conn.transaction().context("begin inventory.create tx")?;

    tx.execute(
        "INSERT INTO items (
            base_kind, name, material, material_tier, quality, quantity,
            holder_character_id, container_item_id, zone_location_id,
            updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            p.base_kind,
            p.name,
            p.material,
            p.material_tier,
            p.quality,
            quantity,
            p.holder_character_id,
            p.container_item_id,
            p.zone_location_id,
            now,
        ],
    )
    .context("insert items row")?;
    let item_id = tx.last_insert_rowid();

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "item.created",
            campaign_hour: now,
            combat_round: None,
            zone_id: p.zone_location_id,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Item {:?} created (id={item_id}, qty={quantity}, base_kind={:?})",
                p.name.as_deref().unwrap_or(p.base_kind.as_str()),
                p.base_kind,
            ),
            payload: serde_json::json!({
                "base_kind": p.base_kind,
                "material": p.material,
                "material_tier": p.material_tier,
                "quality": p.quality,
                "quantity": quantity,
                "holder_character_id": p.holder_character_id,
                "zone_location_id": p.zone_location_id,
                "container_item_id": p.container_item_id,
            }),
            participants: p
                .holder_character_id
                .map(|cid| {
                    [Participant {
                        character_id: cid,
                        role: "actor",
                    }]
                })
                .as_ref()
                .map(|arr| &arr[..])
                .unwrap_or(&[]),
            items: &[crate::events::ItemRef {
                item_id,
                role: "created",
            }],
        },
    )?;

    tx.commit().context("commit inventory.create tx")?;
    Ok(CreateResult {
        item_id,
        event_id: emitted.event_id,
    })
}

/// Pickup flow. Returns `Ok(Err(refused))` if the pickup would overload the character;
/// on success returns `Ok(Ok(result))` with the updated encumbrance numbers.
pub fn pickup(
    conn: &mut Connection,
    content: &Content,
    p: PickupParams,
) -> Result<std::result::Result<PickupResult, PickupRefused>> {
    // Read the item row + its zone location.
    let item = read_item(conn, p.item_id)?;
    let item_zone = item.zone_location_id.ok_or_else(|| {
        anyhow::anyhow!(
            "item {} is not in a zone (holder={:?}, container={:?})",
            p.item_id,
            item.holder_character_id,
            item.container_item_id
        )
    })?;
    let (str_score, current_zone_id): (i32, Option<i64>) = conn
        .query_row(
            "SELECT str_score, current_zone_id FROM characters WHERE id = ?1",
            [p.character_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .with_context(|| format!("character {} not found", p.character_id))?;
    // Co-location check: the picker must be in the same zone as the item. Otherwise any
    // character could pick up any zone-located item anywhere on the map.
    match current_zone_id {
        Some(czid) if czid == item_zone => {}
        other => bail!(
            "cannot pick up item {} (in zone {item_zone}) from character {} (current zone {:?})",
            p.item_id,
            p.character_id,
            other
        ),
    }

    let capacity = (str_score as f64) * (content.encumbrance.capacity_per_str as f64);
    let item_weight = effective_weight_for_item(&item, content);
    let current_carried = carried_weight_lb(conn, content, p.character_id)?;
    let would_be = current_carried + item_weight;
    let would_pct = pct_of_capacity(would_be, capacity);

    // Overload check uses raw weight, not the floored percentage — otherwise a pickup at
    // 150.02 lb / 150 lb (100.01%) would floor to 100 and be accepted. The docs call this
    // "above 100%"; strict on raw pounds is the truthful implementation.
    let overloaded_limit = capacity * (content.encumbrance.overloaded_threshold_pct as f64) / 100.0;
    if would_be > overloaded_limit {
        return Ok(Err(PickupRefused {
            error: "would_overload",
            character_id: p.character_id,
            item_id: p.item_id,
            would_be_weight_lb: would_be,
            capacity_lb: capacity,
        }));
    }

    let now = crate::world::current_campaign_hour(conn)?;

    let tx = conn.transaction().context("begin pickup tx")?;

    // Clear zone location and set holder.
    tx.execute(
        "UPDATE items
         SET holder_character_id = ?1, zone_location_id = NULL,
             updated_at = ?2
         WHERE id = ?3",
        params![p.character_id, now, p.item_id],
    )
    .context("update items location")?;

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "item.pickup",
            campaign_hour: now,
            combat_round: None,
            zone_id: item.zone_location_id,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={} picked up item id={} ({} lb carried of {} capacity)",
                p.character_id, p.item_id, would_be as i64, capacity as i64
            ),
            payload: serde_json::json!({
                "item_id": p.item_id,
                "base_kind": item.base_kind,
                "quantity": item.quantity,
                "weight_lb": item_weight,
                "carried_before": current_carried,
                "carried_after": would_be,
                "capacity_lb": capacity,
                "percent_of_capacity": would_pct,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "actor",
            }],
            items: &[crate::events::ItemRef {
                item_id: p.item_id,
                role: "picked_up",
            }],
        },
    )?;

    let (encumbered_now, encumbered_condition_id) =
        apply_encumbered_in_tx(&tx, content, p.character_id, would_pct, now)?;

    tx.commit().context("commit pickup tx")?;

    Ok(Ok(PickupResult {
        item_id: p.item_id,
        character_id: p.character_id,
        carried_weight_lb: would_be,
        capacity_lb: capacity,
        percent_of_capacity: would_pct,
        encumbered: encumbered_now,
        encumbered_condition_id,
        event_id: emitted.event_id,
    }))
}

pub fn drop_item(conn: &mut Connection, content: &Content, p: DropParams) -> Result<DropResult> {
    let item = read_item(conn, p.item_id)?;
    if item.holder_character_id != Some(p.character_id) {
        bail!(
            "item {} is not held by character {} (holder={:?})",
            p.item_id,
            p.character_id,
            item.holder_character_id
        );
    }
    let zone_id: i64 = conn
        .query_row(
            "SELECT current_zone_id FROM characters WHERE id = ?1",
            [p.character_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .with_context(|| format!("character {} not found", p.character_id))?
        .ok_or_else(|| anyhow::anyhow!("character {} has no current_zone_id", p.character_id))?;

    let str_score: i32 = conn
        .query_row(
            "SELECT str_score FROM characters WHERE id = ?1",
            [p.character_id],
            |row| row.get(0),
        )
        .context("read str_score")?;

    let now = crate::world::current_campaign_hour(conn)?;
    let tx = conn.transaction().context("begin drop tx")?;

    tx.execute(
        "UPDATE items
         SET holder_character_id = NULL, zone_location_id = ?1,
             equipped_slot = NULL, updated_at = ?2
         WHERE id = ?3",
        params![zone_id, now, p.item_id],
    )
    .context("drop item")?;

    // Recompute carried weight after the drop.
    let new_carried: f64 = {
        let mut stmt = tx.prepare(
            "SELECT i.base_kind, i.quantity
             FROM items i
             WHERE i.holder_character_id = ?1",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([p.character_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        rows.iter()
            .map(|(bk, qty)| {
                let base_weight = content
                    .item_bases
                    .get(bk)
                    .map(|b| b.weight_lb)
                    .unwrap_or(0.0);
                base_weight * (*qty as f64)
            })
            .sum()
    };
    let capacity = (str_score as f64) * (content.encumbrance.capacity_per_str as f64);
    let pct = pct_of_capacity(new_carried, capacity);

    let (encumbered_now, _) = apply_encumbered_in_tx(&tx, content, p.character_id, pct, now)?;

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "item.drop",
            campaign_hour: now,
            combat_round: None,
            zone_id: Some(zone_id),
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={} dropped item id={} in zone {zone_id}",
                p.character_id, p.item_id
            ),
            payload: serde_json::json!({
                "item_id": p.item_id,
                "zone_id": zone_id,
                "carried_after": new_carried,
                "capacity_lb": capacity,
                "percent_of_capacity": pct,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "actor",
            }],
            items: &[crate::events::ItemRef {
                item_id: p.item_id,
                role: "dropped",
            }],
        },
    )?;

    tx.commit().context("commit drop tx")?;

    Ok(DropResult {
        item_id: p.item_id,
        zone_id,
        carried_weight_lb: new_carried,
        capacity_lb: capacity,
        percent_of_capacity: pct,
        encumbered: encumbered_now,
        event_id: emitted.event_id,
    })
}

pub fn equip(conn: &mut Connection, _content: &Content, p: EquipParams) -> Result<EquipResult> {
    let item = read_item(conn, p.item_id)?;
    if item.holder_character_id != Some(p.character_id) {
        bail!(
            "cannot equip item {} — it is not held by character {}",
            p.item_id,
            p.character_id
        );
    }
    if p.slot.is_empty() {
        bail!("slot must be non-empty");
    }
    let now = crate::world::current_campaign_hour(conn)?;
    let tx = conn.transaction().context("begin equip tx")?;
    tx.execute(
        "UPDATE items SET equipped_slot = ?1, updated_at = ?2 WHERE id = ?3",
        params![p.slot, now, p.item_id],
    )
    .context("set equipped_slot")?;
    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "item.equipped",
            campaign_hour: now,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={} equipped item id={} to slot {:?}",
                p.character_id, p.item_id, p.slot
            ),
            payload: serde_json::json!({
                "item_id": p.item_id,
                "slot": p.slot,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "actor",
            }],
            items: &[crate::events::ItemRef {
                item_id: p.item_id,
                role: "equipped",
            }],
        },
    )?;
    tx.commit().context("commit equip tx")?;
    Ok(EquipResult {
        item_id: p.item_id,
        slot: Some(p.slot),
        event_id: emitted.event_id,
    })
}

pub fn unequip(conn: &mut Connection, _content: &Content, p: UnequipParams) -> Result<EquipResult> {
    let item = read_item(conn, p.item_id)?;
    if item.holder_character_id != Some(p.character_id) {
        bail!(
            "cannot unequip item {} — it is not held by character {}",
            p.item_id,
            p.character_id
        );
    }
    let now = crate::world::current_campaign_hour(conn)?;
    let tx = conn.transaction().context("begin unequip tx")?;
    tx.execute(
        "UPDATE items SET equipped_slot = NULL, updated_at = ?1 WHERE id = ?2",
        params![now, p.item_id],
    )
    .context("clear equipped_slot")?;
    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "item.unequipped",
            campaign_hour: now,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Character id={} unequipped item id={}",
                p.character_id, p.item_id
            ),
            payload: serde_json::json!({
                "item_id": p.item_id,
            }),
            participants: &[Participant {
                character_id: p.character_id,
                role: "actor",
            }],
            items: &[crate::events::ItemRef {
                item_id: p.item_id,
                role: "unequipped",
            }],
        },
    )?;
    tx.commit().context("commit unequip tx")?;
    Ok(EquipResult {
        item_id: p.item_id,
        slot: None,
        event_id: emitted.event_id,
    })
}

pub fn get(conn: &Connection, content: &Content, character_id: i64) -> Result<InventoryView> {
    let str_score: i32 = conn
        .query_row(
            "SELECT str_score FROM characters WHERE id = ?1",
            [character_id],
            |row| row.get(0),
        )
        .with_context(|| format!("character {character_id} not found"))?;

    let mut stmt = conn.prepare(
        "SELECT id, base_kind, name, material, material_tier, quality, quantity, equipped_slot
         FROM items
         WHERE holder_character_id = ?1
         ORDER BY id",
    )?;
    let items: Vec<ItemView> = stmt
        .query_map([character_id], |row| {
            let base_kind: String = row.get(1)?;
            let quantity: i64 = row.get(6)?;
            let base = content.item_bases.get(&base_kind);
            let weight_per = base.map(|b| b.weight_lb).unwrap_or(0.0);
            let value_per = base.map(|b| b.base_value_gp).unwrap_or(0.0);
            Ok(ItemView {
                id: row.get(0)?,
                base_kind: base_kind.clone(),
                name: row.get(2)?,
                material: row.get(3)?,
                material_tier: row.get(4)?,
                quality: row.get(5)?,
                quantity,
                equipped_slot: row.get(7)?,
                effective_weight_lb: weight_per * (quantity as f64),
                effective_value_gp: value_per * (quantity as f64),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let carried: f64 = items.iter().map(|i| i.effective_weight_lb).sum();
    let capacity = (str_score as f64) * (content.encumbrance.capacity_per_str as f64);
    let pct = pct_of_capacity(carried, capacity);
    let encumbered = pct >= content.encumbrance.encumbered_threshold_pct;

    Ok(InventoryView {
        character_id,
        items,
        carried_weight_lb: carried,
        capacity_lb: capacity,
        percent_of_capacity: pct,
        encumbered,
    })
}

pub fn inspect(conn: &Connection, content: &Content, item_id: i64) -> Result<ItemView> {
    let item = read_item(conn, item_id)?;
    let base = content.item_bases.get(&item.base_kind);
    let weight_per = base.map(|b| b.weight_lb).unwrap_or(0.0);
    let value_per = base.map(|b| b.base_value_gp).unwrap_or(0.0);
    Ok(ItemView {
        id: item_id,
        base_kind: item.base_kind,
        name: item.name,
        material: item.material,
        material_tier: item.material_tier,
        quality: item.quality,
        quantity: item.quantity,
        equipped_slot: item.equipped_slot,
        effective_weight_lb: weight_per * (item.quantity as f64),
        effective_value_gp: value_per * (item.quantity as f64),
    })
}

pub fn transfer(
    conn: &mut Connection,
    content: &Content,
    p: TransferParams,
) -> Result<TransferResult> {
    let count = p.to_character_id.is_some() as u8
        + p.to_container_item_id.is_some() as u8
        + p.to_zone_location_id.is_some() as u8;
    if count != 1 {
        bail!("exactly one destination must be set (got {count})");
    }
    // Reject self-contained cycles — not caught by the DB schema, silently valid in SQL.
    if p.to_container_item_id == Some(p.item_id) {
        bail!("cannot transfer item {} into itself", p.item_id);
    }

    // Validate that the source item exists up front so the error message is precise
    // ("item N does not exist") rather than the FK violation we'd hit at commit time.
    let item = read_item(conn, p.item_id)?;

    // Pre-check the destination so caller-facing errors name the missing referent
    // ("destination character N does not exist") rather than the generic UPDATE
    // failure path. Also detects container chain cycles (A → B → A).
    if let Some(cid) = p.to_character_id {
        let exists: bool = conn
            .query_row("SELECT 1 FROM characters WHERE id = ?1", [cid], |_| {
                Ok(true)
            })
            .optional()?
            .unwrap_or(false);
        if !exists {
            bail!("destination character {cid} does not exist");
        }
    }
    if let Some(ctid) = p.to_container_item_id {
        let exists: bool = conn
            .query_row("SELECT 1 FROM items WHERE id = ?1", [ctid], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if !exists {
            bail!("destination container item {ctid} does not exist");
        }
        check_no_container_cycle(conn, ctid, p.item_id)?;
    }
    if let Some(zid) = p.to_zone_location_id {
        let exists: bool = conn
            .query_row("SELECT 1 FROM zones WHERE id = ?1", [zid], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if !exists {
            bail!("destination zone {zid} does not exist");
        }
    }

    // For character destinations, mirror pickup's overload guard. Without this,
    // transfer is the asymmetric loophole that lets any character receive any item
    // regardless of how heavy it is — silently ignoring the encumbrance contract that
    // pickup enforces. Also pre-compute the post-transfer percentage so the encumbered
    // condition gets the right state inside the tx.
    let dest_char_overload: Option<(i64, f64, f64, i32)> = if let Some(cid) = p.to_character_id {
        let (str_score,): (i32,) = conn
            .query_row(
                "SELECT str_score FROM characters WHERE id = ?1",
                [cid],
                |row| Ok((row.get(0)?,)),
            )
            .with_context(|| format!("character {cid} not found"))?;
        let capacity = (str_score as f64) * (content.encumbrance.capacity_per_str as f64);
        let item_weight = effective_weight_for_item(&item, content);
        // If the item is already held by this character, transferring it back to them is
        // a no-op weight-wise; only count weight when the holder actually changes.
        let weight_delta = if item.holder_character_id == Some(cid) {
            0.0
        } else {
            item_weight
        };
        let current_carried = carried_weight_lb(conn, content, cid)?;
        let would_be = current_carried + weight_delta;
        let overloaded_limit =
            capacity * (content.encumbrance.overloaded_threshold_pct as f64) / 100.0;
        if would_be > overloaded_limit {
            bail!(
                "transfer to character {cid} would overload them ({would_be:.2} lb carried of {capacity:.2} capacity); use inventory.pickup or drop weight first"
            );
        }
        let would_pct = pct_of_capacity(would_be, capacity);
        Some((cid, would_be, capacity, would_pct))
    } else {
        None
    };

    let now = crate::world::current_campaign_hour(conn)?;
    let tx = conn.transaction().context("begin transfer tx")?;
    let updated = tx
        .execute(
            "UPDATE items
             SET holder_character_id = ?1, container_item_id = ?2, zone_location_id = ?3,
                 equipped_slot = NULL, updated_at = ?4
             WHERE id = ?5",
            params![
                p.to_character_id,
                p.to_container_item_id,
                p.to_zone_location_id,
                now,
                p.item_id,
            ],
        )
        .context("transfer item")?;
    if updated == 0 {
        bail!("item {} does not exist", p.item_id);
    }

    let emitted = events::emit_in_tx(
        &tx,
        &EventSpec {
            kind: "item.transfer",
            campaign_hour: now,
            combat_round: None,
            zone_id: p.to_zone_location_id,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Item id={} transferred to {}",
                p.item_id,
                describe_destination(&p)
            ),
            payload: serde_json::json!({
                "item_id": p.item_id,
                "to_character_id": p.to_character_id,
                "to_container_item_id": p.to_container_item_id,
                "to_zone_location_id": p.to_zone_location_id,
            }),
            participants: &match p.to_character_id {
                Some(cid) => vec![Participant {
                    character_id: cid,
                    role: "beneficiary",
                }],
                None => vec![],
            },
            items: &[crate::events::ItemRef {
                item_id: p.item_id,
                role: "transferred",
            }],
        },
    )?;
    // For character destinations, recompute encumbrance inside the same tx so the
    // 'encumbered' condition matches the post-transfer state. Mirrors the pickup path.
    if let Some((cid, _would_be, _capacity, would_pct)) = dest_char_overload {
        apply_encumbered_in_tx(&tx, content, cid, would_pct, now)?;
    }
    // Also re-evaluate the SOURCE holder's encumbrance if the item moved off a holder.
    // Without this, removing weight from a previously-encumbered character wouldn't
    // clear the condition.
    if let Some(prev_holder) = item.holder_character_id {
        if Some(prev_holder) != p.to_character_id {
            // Recompute their carried weight + percentage post-transfer.
            let (str_score,): (i32,) = tx
                .query_row(
                    "SELECT str_score FROM characters WHERE id = ?1",
                    [prev_holder],
                    |row| Ok((row.get(0)?,)),
                )
                .with_context(|| format!("source holder {prev_holder} disappeared"))?;
            let capacity = (str_score as f64) * (content.encumbrance.capacity_per_str as f64);
            let new_carried = carried_weight_lb(&tx, content, prev_holder)?;
            let new_pct = pct_of_capacity(new_carried, capacity);
            apply_encumbered_in_tx(&tx, content, prev_holder, new_pct, now)?;
        }
    }
    tx.commit().context("commit transfer tx")?;
    Ok(TransferResult {
        item_id: p.item_id,
        event_id: emitted.event_id,
    })
}

/// Walk the container chain upward from `start_container`, bailing if `item_being_moved`
/// is already an ancestor (which would create a cycle once the move is applied) or if
/// we exceed a safety depth. Lazy traversal — stops at the first NULL parent or root
/// holder/zone reference.
fn check_no_container_cycle(
    conn: &Connection,
    start_container: i64,
    item_being_moved: i64,
) -> Result<()> {
    let mut current: Option<i64> = Some(start_container);
    let mut steps: u32 = 0;
    while let Some(cid) = current {
        if cid == item_being_moved {
            bail!(
                "transferring item {item_being_moved} into container {start_container} would create a cycle (item is already an ancestor of the container)"
            );
        }
        steps += 1;
        if steps > 32 {
            bail!(
                "container chain depth from {start_container} exceeds 32 — refusing to walk further (probable existing cycle in data)"
            );
        }
        current = conn
            .query_row(
                "SELECT container_item_id FROM items WHERE id = ?1",
                [cid],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
    }
    Ok(())
}

fn describe_destination(p: &TransferParams) -> String {
    if let Some(cid) = p.to_character_id {
        format!("character id={cid}")
    } else if let Some(cnt) = p.to_container_item_id {
        format!("container id={cnt}")
    } else if let Some(z) = p.to_zone_location_id {
        format!("zone id={z}")
    } else {
        "nowhere".to_string()
    }
}

// ── Internals ────────────────────────────────────────────────────────────────

pub(crate) struct ItemRow {
    pub base_kind: String,
    #[allow(dead_code)]
    pub name: Option<String>,
    #[allow(dead_code)]
    pub material: Option<String>,
    #[allow(dead_code)]
    pub material_tier: Option<i32>,
    #[allow(dead_code)]
    pub quality: Option<String>,
    pub quantity: i64,
    pub holder_character_id: Option<i64>,
    pub container_item_id: Option<i64>,
    pub zone_location_id: Option<i64>,
    #[allow(dead_code)]
    pub equipped_slot: Option<String>,
}

pub(crate) fn read_item(conn: &Connection, item_id: i64) -> Result<ItemRow> {
    conn.query_row(
        "SELECT base_kind, name, material, material_tier, quality, quantity,
                holder_character_id, container_item_id, zone_location_id, equipped_slot
         FROM items WHERE id = ?1",
        [item_id],
        |row| {
            Ok(ItemRow {
                base_kind: row.get(0)?,
                name: row.get(1)?,
                material: row.get(2)?,
                material_tier: row.get(3)?,
                quality: row.get(4)?,
                quantity: row.get(5)?,
                holder_character_id: row.get(6)?,
                container_item_id: row.get(7)?,
                zone_location_id: row.get(8)?,
                equipped_slot: row.get(9)?,
            })
        },
    )
    .with_context(|| format!("item {item_id} not found"))
}

pub(crate) fn effective_weight_for_item(item: &ItemRow, content: &Content) -> f64 {
    let base = match content.item_bases.get(&item.base_kind) {
        Some(b) => b,
        None => return 0.0,
    };
    base.weight_lb * (item.quantity as f64)
}

pub(crate) fn effective_value_for_base(base: &BaseItem, quantity: i64) -> f64 {
    base.base_value_gp * (quantity as f64)
}

fn pct_of_capacity(weight_lb: f64, capacity_lb: f64) -> i32 {
    if capacity_lb <= 0.0 {
        return 100;
    }
    ((weight_lb * 100.0) / capacity_lb).floor() as i32
}

/// Total weight of items directly held by this character. Phase 10 ignores container
/// nesting — container items and their contents aren't separately modelled yet.
pub(crate) fn carried_weight_lb(
    conn: &Connection,
    content: &Content,
    character_id: i64,
) -> Result<f64> {
    let mut stmt =
        conn.prepare("SELECT base_kind, quantity FROM items WHERE holder_character_id = ?1")?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([character_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    Ok(rows
        .iter()
        .map(|(bk, qty)| {
            content
                .item_bases
                .get(bk)
                .map(|b| b.weight_lb)
                .unwrap_or(0.0)
                * (*qty as f64)
        })
        .sum())
}

/// Apply or clear the `encumbered` condition based on the character's current load.
/// Returns (encumbered_now, optional_condition_id_if_applied_this_call).
fn apply_encumbered_in_tx(
    tx: &rusqlite::Transaction<'_>,
    content: &Content,
    character_id: i64,
    pct: i32,
    now: i64,
) -> Result<(bool, Option<i64>)> {
    // Encumbered band: [encumbered_threshold_pct, overloaded_threshold_pct]. Inclusive on
    // both ends so a character at exactly 100% capacity is encumbered but not overloaded.
    let should_be_encumbered = pct >= content.encumbrance.encumbered_threshold_pct
        && pct <= content.encumbrance.overloaded_threshold_pct;
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM character_conditions
             WHERE character_id = ?1 AND condition = 'encumbered' AND active = 1
             ORDER BY id DESC LIMIT 1",
            [character_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("query existing encumbered condition")?;

    match (should_be_encumbered, existing) {
        (true, Some(_)) => Ok((true, None)),
        (true, None) => {
            // Apply the condition. Emit condition.applied + insert character_conditions row.
            let emitted = events::emit_in_tx(
                tx,
                &EventSpec {
                    kind: "condition.applied",
                    campaign_hour: now,
                    combat_round: None,
                    zone_id: None,
                    encounter_id: None,
                    parent_id: None,
                    summary: format!(
                        "Character id={character_id} became encumbered ({pct}% of capacity)"
                    ),
                    payload: serde_json::json!({
                        "condition": "encumbered",
                        "severity": 1,
                        "percent_of_capacity": pct,
                    }),
                    participants: &[Participant {
                        character_id,
                        role: "target",
                    }],
                    items: &[],
                },
            )?;
            tx.execute(
                "INSERT INTO character_conditions
                    (character_id, condition, severity, source_event_id, active)
                 VALUES (?1, 'encumbered', 1, ?2, 1)",
                params![character_id, emitted.event_id],
            )
            .context("insert encumbered condition row")?;
            Ok((true, Some(tx.last_insert_rowid())))
        }
        (false, Some(cond_id)) => {
            tx.execute(
                "UPDATE character_conditions SET active = 0 WHERE id = ?1",
                [cond_id],
            )?;
            events::emit_in_tx(
                tx,
                &EventSpec {
                    kind: "condition.expired",
                    campaign_hour: now,
                    combat_round: None,
                    zone_id: None,
                    encounter_id: None,
                    parent_id: None,
                    summary: format!(
                        "Character id={character_id} no longer encumbered ({pct}% of capacity)"
                    ),
                    payload: serde_json::json!({
                        "condition_id": cond_id,
                        "condition": "encumbered",
                        "reason": "weight_dropped_below_threshold",
                        "percent_of_capacity": pct,
                    }),
                    participants: &[Participant {
                        character_id,
                        role: "target",
                    }],
                    items: &[],
                },
            )?;
            Ok((false, None))
        }
        (false, None) => Ok((false, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::characters::{self, CreateParams as CharCreateParams};
    use crate::db::schema;
    use rusqlite::Connection;

    fn fresh() -> (Connection, Content) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        (conn, Content::load(None).unwrap())
    }

    fn make_char(conn: &mut Connection, name: &str, str_score: i32) -> i64 {
        characters::create(
            conn,
            CharCreateParams {
                name: name.into(),
                role: "player".into(),
                str_score,
                dex_score: 10,
                con_score: 10,
                int_score: 10,
                wis_score: 10,
                cha_score: 10,
                hp_max: Some(20),
                hp_current: Some(20),
                armor_class: Some(12),
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

    fn make_zone(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO zones (name, biome, kind, size) VALUES ('Z', 'plains', 'wilderness', 'small')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn pickup_respects_capacity_thresholds() {
        let (mut conn, content) = fresh();
        let pc = make_char(&mut conn, "Kira", 10); // capacity = 150 lb
        let zone = make_zone(&conn);
        conn.execute(
            "UPDATE characters SET current_zone_id = ?1 WHERE id = ?2",
            params![zone, pc],
        )
        .unwrap();

        // Pickup a 100-lb crate → 66% (not encumbered yet).
        let crate_id = create(
            &mut conn,
            &content,
            CreateParams {
                base_kind: "heavy_crate".into(),
                name: None,
                material: None,
                material_tier: None,
                quality: None,
                quantity: None,
                holder_character_id: None,
                zone_location_id: Some(zone),
                container_item_id: None,
            },
        )
        .unwrap()
        .item_id;
        let r = pickup(
            &mut conn,
            &content,
            PickupParams {
                character_id: pc,
                item_id: crate_id,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.percent_of_capacity, 66);
        assert!(!r.encumbered);

        // Pickup a 10-lb stone → 110/150 = 73% → encumbered.
        let stone_id = create(
            &mut conn,
            &content,
            CreateParams {
                base_kind: "stone".into(),
                name: None,
                material: None,
                material_tier: None,
                quality: None,
                quantity: None,
                holder_character_id: None,
                zone_location_id: Some(zone),
                container_item_id: None,
            },
        )
        .unwrap()
        .item_id;
        let r = pickup(
            &mut conn,
            &content,
            PickupParams {
                character_id: pc,
                item_id: stone_id,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.percent_of_capacity, 73);
        assert!(r.encumbered);
        assert!(r.encumbered_condition_id.is_some());

        // Try to pickup 5 stones (50 lb) → would be 160 → refused.
        let extra_stones: Vec<i64> = (0..5)
            .map(|_| {
                create(
                    &mut conn,
                    &content,
                    CreateParams {
                        base_kind: "stone".into(),
                        name: None,
                        material: None,
                        material_tier: None,
                        quality: None,
                        quantity: None,
                        holder_character_id: None,
                        zone_location_id: Some(zone),
                        container_item_id: None,
                    },
                )
                .unwrap()
                .item_id
            })
            .collect();
        // Pickup first four fine.
        for sid in &extra_stones[..4] {
            pickup(
                &mut conn,
                &content,
                PickupParams {
                    character_id: pc,
                    item_id: *sid,
                },
            )
            .unwrap()
            .unwrap();
        }
        // Fifth pushes us to 160 lb → refused.
        let refused = pickup(
            &mut conn,
            &content,
            PickupParams {
                character_id: pc,
                item_id: extra_stones[4],
            },
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(refused.error, "would_overload");
        assert_eq!(refused.would_be_weight_lb, 160.0);
        assert_eq!(refused.capacity_lb, 150.0);
    }

    #[test]
    fn drop_clears_encumbered_when_below_threshold() {
        let (mut conn, content) = fresh();
        let pc = make_char(&mut conn, "K", 10);
        let zone = make_zone(&conn);
        conn.execute(
            "UPDATE characters SET current_zone_id = ?1 WHERE id = ?2",
            params![zone, pc],
        )
        .unwrap();
        // Weigh in at 110 lb → encumbered.
        let c1 = create(
            &mut conn,
            &content,
            CreateParams {
                base_kind: "heavy_crate".into(),
                name: None,
                material: None,
                material_tier: None,
                quality: None,
                quantity: None,
                holder_character_id: None,
                zone_location_id: Some(zone),
                container_item_id: None,
            },
        )
        .unwrap()
        .item_id;
        let stone = create(
            &mut conn,
            &content,
            CreateParams {
                base_kind: "stone".into(),
                name: None,
                material: None,
                material_tier: None,
                quality: None,
                quantity: None,
                holder_character_id: None,
                zone_location_id: Some(zone),
                container_item_id: None,
            },
        )
        .unwrap()
        .item_id;
        pickup(
            &mut conn,
            &content,
            PickupParams {
                character_id: pc,
                item_id: c1,
            },
        )
        .unwrap()
        .unwrap();
        let r = pickup(
            &mut conn,
            &content,
            PickupParams {
                character_id: pc,
                item_id: stone,
            },
        )
        .unwrap()
        .unwrap();
        assert!(r.encumbered);

        // Drop the stone → back to 100 lb = 66% → not encumbered.
        let r = drop_item(
            &mut conn,
            &content,
            DropParams {
                character_id: pc,
                item_id: stone,
            },
        )
        .unwrap();
        assert!(!r.encumbered);
        let active: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM character_conditions
                 WHERE character_id = ?1 AND condition = 'encumbered' AND active = 1",
                [pc],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, 0);
    }
}
