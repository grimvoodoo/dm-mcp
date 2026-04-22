//! Barter: items-for-items (and gold) trades with persuasion-check-driven rate.
//!
//! `barter.exchange` computes the total `effective_value_gp` of each side's items, decides
//! whether the merchant will accept the deal outright, and otherwise rolls a persuasion
//! check against a DC derived from the value mismatch. A below-fair-value offer becomes
//! possible if the player rolls well.
//!
//! Design notes (per `docs/items.md §Barter`):
//!
//! - `fair_ratio` (content-configurable) governs the no-check acceptance band. If the
//!   offered value is at least `fair_ratio * requested` (default 0.9), the deal completes
//!   without a roll.
//! - Below the fair band, `resolve_check(persuasion, dc)` is rolled on the player. DC
//!   scales with the size of the discount. A manifestly bad deal (offered < `refuse_ratio`,
//!   default 0.5, times requested) is refused outright before any roll.
//! - On success: both inventories update atomically and `social.bargain` is emitted.
//! - On failure: no state changes beyond the emitted `social.bargain` with
//!   `outcome='declined'`.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::checks::{self, ResolveCheckParams};
use crate::content::Content;
use crate::events::{self, EventSpec, ItemRef, Participant};
use crate::inventory::{effective_value_for_base, read_item};

// ── Params / results ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ExchangeParams {
    /// Character offering items.
    pub character_id: i64,
    /// Merchant / counterparty.
    pub merchant_character_id: i64,
    /// Item ids the player is offering. Must all be held by `character_id`.
    pub offered_item_ids: Vec<i64>,
    /// Item ids the player is requesting. Must all be held by `merchant_character_id`.
    pub requested_item_ids: Vec<i64>,
    /// Optional persuasion-DC override for tests and explicit DM control. If omitted the
    /// DC is derived from the value ratio (the normal flow).
    #[serde(default)]
    pub dc_override: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExchangeResult {
    pub outcome: String,
    pub offered_value_gp: f64,
    pub requested_value_gp: f64,
    /// "auto_accept" | "persuasion_check" | "refused"
    pub resolution: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_dc: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_roll_total: Option<i64>,
    pub event_id: i64,
}

// ── Tunables (content-driven later; hard-coded for Phase 10 MVP) ─────────────

const FAIR_RATIO: f64 = 0.9; // offered / requested ≥ this → auto-accept
const REFUSE_RATIO: f64 = 0.5; // offered / requested < this → merchant refuses outright
const DC_BASE: i32 = 10;
const DC_PER_10PCT_GAP: i32 = 2;

// ── Implementation ───────────────────────────────────────────────────────────

pub fn exchange(
    conn: &mut Connection,
    content: &Content,
    p: ExchangeParams,
) -> Result<ExchangeResult> {
    // Both sides must contain at least one item — otherwise (especially with dc_override)
    // a zero-item offer can enter the persuasion path and quietly win merchant goods.
    if p.offered_item_ids.is_empty() || p.requested_item_ids.is_empty() {
        bail!(
            "barter.exchange requires at least one item on each side (got offered={}, requested={})",
            p.offered_item_ids.len(),
            p.requested_item_ids.len()
        );
    }
    // Duplicate IDs would double-count values and move the row once — reject both sides.
    ensure_unique("offered_item_ids", &p.offered_item_ids)?;
    ensure_unique("requested_item_ids", &p.requested_item_ids)?;
    if p.character_id == p.merchant_character_id {
        bail!("cannot barter with oneself");
    }

    // Validate + sum values for both sides.
    let offered_value = sum_side_value(conn, content, p.character_id, &p.offered_item_ids)?;
    let requested_value = sum_side_value(
        conn,
        content,
        p.merchant_character_id,
        &p.requested_item_ids,
    )?;

    let ratio = if requested_value > 0.0 {
        offered_value / requested_value
    } else {
        f64::INFINITY
    };

    // Decision: refuse outright, auto-accept, or roll persuasion. Path + DC + roll are
    // all computed before we open the transaction so the tx body is purely DB writes.
    let now = crate::world::current_campaign_hour(conn)?;
    let path = decide_barter_path(&p, ratio);
    let (outcome, resolution, check_dc, check_roll) = match path {
        BarterPath::Refused => ("declined", "refused", None, None),
        BarterPath::AutoAccept => ("accepted", "auto_accept", None, None),
        BarterPath::PersuasionCheck => {
            let dc = p.dc_override.unwrap_or_else(|| derive_dc(ratio));
            let check = checks::resolve(
                conn,
                content,
                ResolveCheckParams {
                    character_id: p.character_id,
                    kind: "skill_check".into(),
                    target_key: "persuasion".into(),
                    ability: None,
                    dc: Some(dc),
                    target_character_id: Some(p.merchant_character_id),
                    modifiers: Vec::new(),
                    advantage: None,
                    disadvantage: None,
                },
            )
            .context("resolve persuasion check for barter")?;
            let success = check.success.unwrap_or(false);
            (
                if success { "accepted" } else { "declined" },
                "persuasion_check",
                Some(dc),
                Some(check.total),
            )
        }
    };

    // Atomic: swap (if accepted) + social.bargain in one transaction. If event insertion
    // fails for any reason, the swap rolls back and no state changes.
    let tx = conn.transaction().context("begin barter tx")?;
    if outcome == "accepted" {
        swap_inventories_in_tx(&tx, &p, now)?;
    }
    let event_id = emit_bargain_event_in_tx(
        &tx,
        &p,
        offered_value,
        requested_value,
        outcome,
        resolution,
        check_dc,
        check_roll,
        now,
    )?;
    tx.commit().context("commit barter tx")?;

    Ok(ExchangeResult {
        outcome: outcome.to_string(),
        offered_value_gp: offered_value,
        requested_value_gp: requested_value,
        resolution: resolution.to_string(),
        check_dc,
        check_roll_total: check_roll,
        event_id,
    })
}

enum BarterPath {
    Refused,
    AutoAccept,
    PersuasionCheck,
}

fn decide_barter_path(p: &ExchangeParams, ratio: f64) -> BarterPath {
    // dc_override always takes the persuasion path (DMs use it for scripted checks; tests
    // use it for forced outcomes).
    if p.dc_override.is_some() {
        return BarterPath::PersuasionCheck;
    }
    if ratio < REFUSE_RATIO {
        BarterPath::Refused
    } else if ratio >= FAIR_RATIO {
        BarterPath::AutoAccept
    } else {
        BarterPath::PersuasionCheck
    }
}

// ── Internals ────────────────────────────────────────────────────────────────

fn ensure_unique(label: &str, ids: &[i64]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(*id) {
            bail!("{label} contains duplicate id {id}");
        }
    }
    Ok(())
}

fn sum_side_value(
    conn: &Connection,
    content: &Content,
    holder_id: i64,
    item_ids: &[i64],
) -> Result<f64> {
    let mut total = 0.0;
    for iid in item_ids {
        let item = read_item(conn, *iid)?;
        if item.holder_character_id != Some(holder_id) {
            bail!(
                "item {iid} is not held by character {holder_id} (holder={:?})",
                item.holder_character_id
            );
        }
        let base = content.item_bases.get(&item.base_kind).ok_or_else(|| {
            anyhow::anyhow!("item {iid} has unknown base_kind {:?}", item.base_kind)
        })?;
        total += effective_value_for_base(base, item.quantity);
    }
    Ok(total)
}

/// Swap both item sets inside the caller's transaction so the moves and the
/// `social.bargain` emission are atomic. If any UPDATE leaves zero rows (e.g. the id was
/// stale) we bail before committing — the caller rolls the tx back.
fn swap_inventories_in_tx(
    tx: &rusqlite::Transaction<'_>,
    p: &ExchangeParams,
    now: i64,
) -> Result<()> {
    for iid in &p.offered_item_ids {
        let n = tx
            .execute(
                "UPDATE items
             SET holder_character_id = ?1, zone_location_id = NULL, container_item_id = NULL,
                 equipped_slot = NULL, updated_at = ?2
             WHERE id = ?3 AND holder_character_id = ?4",
                params![p.merchant_character_id, now, *iid, p.character_id],
            )
            .with_context(|| format!("move offered item {iid}"))?;
        if n == 0 {
            bail!(
                "offered item {iid} is no longer held by character {}",
                p.character_id
            );
        }
    }
    for iid in &p.requested_item_ids {
        let n = tx
            .execute(
                "UPDATE items
             SET holder_character_id = ?1, zone_location_id = NULL, container_item_id = NULL,
                 equipped_slot = NULL, updated_at = ?2
             WHERE id = ?3 AND holder_character_id = ?4",
                params![p.character_id, now, *iid, p.merchant_character_id],
            )
            .with_context(|| format!("move requested item {iid}"))?;
        if n == 0 {
            bail!(
                "requested item {iid} is no longer held by merchant {}",
                p.merchant_character_id
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_bargain_event_in_tx(
    tx: &rusqlite::Transaction<'_>,
    p: &ExchangeParams,
    offered: f64,
    requested: f64,
    outcome: &str,
    resolution: &str,
    check_dc: Option<i32>,
    roll_total: Option<i64>,
    now: i64,
) -> Result<i64> {
    let participants = [
        Participant {
            character_id: p.character_id,
            role: "actor",
        },
        Participant {
            character_id: p.merchant_character_id,
            role: "target",
        },
    ];
    let item_refs: Vec<ItemRef<'_>> = p
        .offered_item_ids
        .iter()
        .map(|iid| ItemRef {
            item_id: *iid,
            role: "offered",
        })
        .chain(p.requested_item_ids.iter().map(|iid| ItemRef {
            item_id: *iid,
            role: "requested",
        }))
        .collect();
    let emitted = events::emit_in_tx(
        tx,
        &EventSpec {
            kind: "social.bargain",
            campaign_hour: now,
            combat_round: None,
            zone_id: None,
            encounter_id: None,
            parent_id: None,
            summary: format!(
                "Barter between character id={} and merchant id={}: {outcome} via {resolution} (offered {offered:.1} gp vs requested {requested:.1} gp)",
                p.character_id, p.merchant_character_id
            ),
            payload: serde_json::json!({
                "offered_value_gp": offered,
                "requested_value_gp": requested,
                "outcome": outcome,
                "resolution": resolution,
                "check_dc": check_dc,
                "check_roll_total": roll_total,
                "offered_item_ids": p.offered_item_ids,
                "requested_item_ids": p.requested_item_ids,
            }),
            participants: &participants,
            items: &item_refs,
        },
    )?;
    Ok(emitted.event_id)
}

fn derive_dc(ratio: f64) -> i32 {
    // Ratio 0.9 → DC 10. Each 10% gap below 0.9 adds 2. Ratio 0.5 → DC 18.
    let gap = (FAIR_RATIO - ratio).max(0.0);
    DC_BASE + ((gap * 100.0 / 10.0).ceil() as i32) * DC_PER_10PCT_GAP
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::characters::{self, CreateParams as CharCreateParams};
    use crate::db::schema;
    use crate::inventory::{create as create_item, CreateParams as ItemCreate};

    fn fresh() -> (Connection, Content) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&mut conn).unwrap();
        (conn, Content::load(None).unwrap())
    }

    fn make_char(conn: &mut Connection, name: &str, role: &str, cha: i32) -> i64 {
        characters::create(
            conn,
            CharCreateParams {
                name: name.into(),
                role: role.into(),
                str_score: 10,
                dex_score: 10,
                con_score: 10,
                int_score: 10,
                wis_score: 10,
                cha_score: cha,
                hp_max: Some(10),
                hp_current: Some(10),
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

    fn make_item(
        conn: &mut Connection,
        content: &Content,
        base: &str,
        holder: i64,
        qty: i64,
    ) -> i64 {
        create_item(
            conn,
            content,
            ItemCreate {
                base_kind: base.into(),
                name: None,
                material: None,
                material_tier: None,
                quality: None,
                quantity: Some(qty),
                holder_character_id: Some(holder),
                zone_location_id: None,
                container_item_id: None,
            },
        )
        .unwrap()
        .item_id
    }

    #[test]
    fn fair_offer_auto_accepts_and_swaps() {
        let (mut conn, content) = fresh();
        let pc = make_char(&mut conn, "P", "player", 14);
        let merchant = make_char(&mut conn, "M", "neutral", 10);
        // Player offers a heavy_crate (5 gp) for a stone (0 gp) — gp-ratio here means the
        // crate is worth way more than the stone → auto-accept (player offering surplus).
        let offered = make_item(&mut conn, &content, "heavy_crate", pc, 1);
        let requested = make_item(&mut conn, &content, "stone", merchant, 1);
        let r = exchange(
            &mut conn,
            &content,
            ExchangeParams {
                character_id: pc,
                merchant_character_id: merchant,
                offered_item_ids: vec![offered],
                requested_item_ids: vec![requested],
                dc_override: None,
            },
        )
        .unwrap();
        assert_eq!(r.outcome, "accepted");
        assert_eq!(r.resolution, "auto_accept");
        // Items actually swapped holders.
        let crate_holder: i64 = conn
            .query_row(
                "SELECT holder_character_id FROM items WHERE id = ?1",
                [offered],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(crate_holder, merchant);
        let stone_holder: i64 = conn
            .query_row(
                "SELECT holder_character_id FROM items WHERE id = ?1",
                [requested],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stone_holder, pc);
    }

    #[test]
    fn low_offer_triggers_persuasion_with_forced_dc_success() {
        let (mut conn, content) = fresh();
        let pc = make_char(&mut conn, "P", "player", 18); // cha 18 → +4
        let merchant = make_char(&mut conn, "M", "neutral", 10);
        // Player offers 10 gold (10 gp) for a heavy_crate (5 gp). Wait — player-offered
        // MORE than requested, which is fine — auto-accept. Flip it:
        // Player offers 1 gold for a heavy_crate (5 gp): ratio 0.2 → refuse outright.
        let gold = make_item(&mut conn, &content, "gold", pc, 1);
        let crate_id = make_item(&mut conn, &content, "heavy_crate", merchant, 1);
        // Use dc_override to force the persuasion path + DC=1 → always success.
        let r = exchange(
            &mut conn,
            &content,
            ExchangeParams {
                character_id: pc,
                merchant_character_id: merchant,
                offered_item_ids: vec![gold],
                requested_item_ids: vec![crate_id],
                dc_override: Some(1),
            },
        )
        .unwrap();
        assert_eq!(r.resolution, "persuasion_check");
        // DC=1 + natural d20 + CHA bonus → always success.
        assert_eq!(r.outcome, "accepted");
        let crate_holder: i64 = conn
            .query_row(
                "SELECT holder_character_id FROM items WHERE id = ?1",
                [crate_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(crate_holder, pc);
    }

    #[test]
    fn impossible_dc_fails_and_no_swap() {
        let (mut conn, content) = fresh();
        let pc = make_char(&mut conn, "P", "player", 10);
        let merchant = make_char(&mut conn, "M", "neutral", 10);
        let gold = make_item(&mut conn, &content, "gold", pc, 1);
        let crate_id = make_item(&mut conn, &content, "heavy_crate", merchant, 1);
        let r = exchange(
            &mut conn,
            &content,
            ExchangeParams {
                character_id: pc,
                merchant_character_id: merchant,
                offered_item_ids: vec![gold],
                requested_item_ids: vec![crate_id],
                dc_override: Some(999), // impossible
            },
        )
        .unwrap();
        assert_eq!(r.resolution, "persuasion_check");
        assert_eq!(r.outcome, "declined");
        let crate_holder: i64 = conn
            .query_row(
                "SELECT holder_character_id FROM items WHERE id = ?1",
                [crate_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            crate_holder, merchant,
            "failed persuasion must not move items"
        );
    }

    #[test]
    fn empty_side_is_rejected_even_with_dc_override() {
        // Regression: without the empty-side guard, dc_override could drive the persuasion
        // path on a zero-item offer and rob the merchant on a good roll.
        let (mut conn, content) = fresh();
        let pc = make_char(&mut conn, "P", "player", 14);
        let merchant = make_char(&mut conn, "M", "neutral", 10);
        let crate_id = make_item(&mut conn, &content, "heavy_crate", merchant, 1);
        let err = exchange(
            &mut conn,
            &content,
            ExchangeParams {
                character_id: pc,
                merchant_character_id: merchant,
                offered_item_ids: vec![],
                requested_item_ids: vec![crate_id],
                dc_override: Some(1),
            },
        )
        .expect_err("empty offered side must be rejected");
        assert!(
            format!("{err:#}").contains("at least one item on each side"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn duplicate_item_ids_are_rejected() {
        // Regression: without the dedup guard, duplicates would double-count value and
        // only move the row once.
        let (mut conn, content) = fresh();
        let pc = make_char(&mut conn, "P", "player", 10);
        let merchant = make_char(&mut conn, "M", "neutral", 10);
        let gold = make_item(&mut conn, &content, "gold", pc, 1);
        let crate_id = make_item(&mut conn, &content, "heavy_crate", merchant, 1);
        let err = exchange(
            &mut conn,
            &content,
            ExchangeParams {
                character_id: pc,
                merchant_character_id: merchant,
                offered_item_ids: vec![gold, gold],
                requested_item_ids: vec![crate_id],
                dc_override: None,
            },
        )
        .expect_err("duplicate offered ids must be rejected");
        assert!(format!("{err:#}").contains("duplicate"), "got: {err:#}");
    }

    #[test]
    fn manifestly_bad_offer_refused_without_check() {
        let (mut conn, content) = fresh();
        let pc = make_char(&mut conn, "P", "player", 10);
        let merchant = make_char(&mut conn, "M", "neutral", 10);
        // 1 gold (1gp) for 10 heavy_crates (50gp): ratio 0.02 → manifestly bad → refuse.
        let gold = make_item(&mut conn, &content, "gold", pc, 1);
        let crates: Vec<i64> = (0..10)
            .map(|_| make_item(&mut conn, &content, "heavy_crate", merchant, 1))
            .collect();
        let r = exchange(
            &mut conn,
            &content,
            ExchangeParams {
                character_id: pc,
                merchant_character_id: merchant,
                offered_item_ids: vec![gold],
                requested_item_ids: crates.clone(),
                dc_override: None,
            },
        )
        .unwrap();
        assert_eq!(r.resolution, "refused");
        assert_eq!(r.outcome, "declined");
    }
}
