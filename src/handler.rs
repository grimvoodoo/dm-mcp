//! MCP server handler. Transport-agnostic: the same handler instance is attached to either
//! the stdio or HTTP transport.
//!
//! Phase 1 registered `server.info`. Phase 2 adds `content.introspect`. Later phases add
//! the full tool surface via the same `#[tool_router]` pattern.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::barter::{self, ExchangeParams as BarterExchangeParams};
use crate::characters::{
    self, ChangeRoleParams, CreateParams as CharCreateParams, GetParams, UpdatePlansParams,
};
use crate::checks::{self, ResolveCheckParams};
use crate::combat::{
    self, ApplyDamageParams, ApplyHealingParams, DeathEventParams, DeathSaveParams,
    EndParams as CombatEndParams, NextTurnParams as CombatNextTurnParams,
    StartParams as CombatStartParams,
};
use crate::conditions::{self, ApplyConditionParams, RemoveConditionParams};
use crate::content::Content as ContentCatalog;
use crate::db::DbHandle;
use crate::dice;
use crate::effects::{self, ApplyParams as EffectApplyParams, DispelParams};
use crate::encounters::{
    self, AbandonParams as EncAbandonParams, CompleteParams as EncCompleteParams,
    CreateParams as EncCreateParams, FailParams as EncFailParams,
};
use crate::inventory::{
    self, CreateParams as InvCreateParams, DropParams, EquipParams, GetParams as InvGetParams,
    InspectParams as InvInspectParams, PickupParams, TransferParams as InvTransferParams,
    UnequipParams,
};
use crate::npcs::{self, GenerateParams as NpcGenerateParams, RecallParams as NpcRecallParams};
use crate::proficiencies::{
    self, AdjustResourceParams, RemoveProficiencyParams, RemoveResourceParams,
    SetProficiencyParams, SetResourceParams,
};
use crate::rests::{self, LongRestParams, ShortRestParams};
use crate::setup::{
    self, AnswerParams as SetupAnswerParams, GenerateWorldParams, MarkReadyParams,
    NewCampaignParams,
};
use crate::world::{self, DescribeZoneParams, MapParams, TravelParams};

/// Arguments for the `dice.roll` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiceRollParams {
    /// Dice notation. Accepted shapes: `d4`/`d6`/`d8`/`d10`/`d12`/`d20`/`d100` (single die),
    /// `3d6` (count × sides), or `11-43` (inclusive integer range).
    pub spec: String,
}

/// Small helper — take a mutex on the DB, run a callback, serialise the result as JSON
/// inside a CallToolResult. Keeps tool bodies tight.
fn with_db_mut<F, T>(db: &DbHandle, f: F) -> Result<CallToolResult, McpError>
where
    F: FnOnce(&mut rusqlite::Connection) -> anyhow::Result<T>,
    T: Serialize,
{
    let value = {
        let mut conn = db
            .lock()
            .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
        f(&mut conn).map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
    };
    let json = serde_json::to_string(&value)
        .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn with_db<F, T>(db: &DbHandle, f: F) -> Result<CallToolResult, McpError>
where
    F: FnOnce(&rusqlite::Connection) -> anyhow::Result<T>,
    T: Serialize,
{
    let value = {
        let conn = db
            .lock()
            .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
        f(&conn).map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
    };
    let json = serde_json::to_string(&value)
        .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// The transport this server instance is currently serving. Reported by `server.info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Stdio,
    Http,
}

impl Transport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
        }
    }
}

#[derive(Debug, Serialize)]
struct ServerInfoPayload {
    name: &'static str,
    version: &'static str,
    transport: &'static str,
}

/// The single server handler type, shared across transports.
///
/// `tool_router` is consumed by the `#[tool_handler]` macro expansion to route incoming tool
/// calls — the compiler can't see through the macro, so an explicit `#[allow(dead_code)]`
/// keeps clippy `-D warnings` happy.
#[derive(Clone)]
pub struct DmMcpHandler {
    transport: Transport,
    content: Arc<ContentCatalog>,
    db: DbHandle,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl DmMcpHandler {
    pub fn new(transport: Transport, content: Arc<ContentCatalog>, db: DbHandle) -> Self {
        Self {
            transport,
            content,
            db,
            tool_router: Self::tool_router(),
        }
    }

    /// Return server metadata. Phase 1's sanity-check tool; proves dispatch is wired.
    #[tool(
        name = "server.info",
        description = "Return server name, version, and the transport currently serving this session."
    )]
    async fn server_info(&self) -> Result<CallToolResult, McpError> {
        let payload = ServerInfoPayload {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            transport: self.transport.as_str(),
        };
        let json = serde_json::to_string(&payload).map_err(|e| {
            McpError::internal_error(
                format!("failed to serialize server.info payload: {e}"),
                None,
            )
        })?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Return a structured summary of every content section loaded at startup — one list
    /// of IDs per section. Lets the DM agent confirm which catalog it's running against.
    #[tool(
        name = "content.introspect",
        description = "Return the IDs of every entry loaded from the bundled YAML content catalog (abilities, skills, damage types, conditions, biomes, weapons, enchantments, archetypes)."
    )]
    async fn content_introspect(&self) -> Result<CallToolResult, McpError> {
        let introspection = self.content.introspect();
        let json = serde_json::to_string(&introspection).map_err(|e| {
            McpError::internal_error(
                format!("failed to serialize content.introspect payload: {e}"),
                None,
            )
        })?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Roll dice notation. Supports `d4..=d100`, `NdM` multi-dice, and inclusive ranges
    /// (`11-43`). Returns the total plus the individual rolls so the DM agent can narrate
    /// "you rolled a 4 and a 6" rather than just the sum.
    #[tool(
        name = "dice.roll",
        description = "Roll dice notation. Accepts standard dice (d4, d6, d8, d10, d12, d20, d100), multi-dice (NdM, e.g. 3d6), or an inclusive integer range (e.g. 11-43). Returns {spec, total, rolls: [...]}."
    )]
    async fn dice_roll(
        &self,
        Parameters(DiceRollParams { spec }): Parameters<DiceRollParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = dice::roll(&spec)
            .map_err(|e| McpError::invalid_params(format!("dice.roll: {e:#}"), None))?;
        let json = serde_json::to_string(&result).map_err(|e| {
            McpError::internal_error(format!("failed to serialize dice.roll payload: {e}"), None)
        })?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    // ── Character CRUD ────────────────────────────────────────────────────────

    #[tool(
        name = "character.create",
        description = "Create a new character (player, companion, pet, friendly NPC, or enemy). Takes name + role + six ability scores plus optional combat/label fields. Returns {character_id, event_id}."
    )]
    async fn character_create(
        &self,
        Parameters(params): Parameters<CharCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| characters::create(conn, params))
    }

    #[tool(
        name = "character.get",
        description = "Read a character's full state: base + effective ability scores (with active effects composed), HP/AC/speed, proficiencies, resources, active effects, active conditions."
    )]
    async fn character_get(
        &self,
        Parameters(GetParams { character_id }): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db(&self.db, |conn| characters::get(conn, character_id))
    }

    #[tool(
        name = "character.update_plans",
        description = "Replace a character's `plans` prose (their current agenda / motivations). Emits npc.plan_changed."
    )]
    async fn character_update_plans(
        &self,
        Parameters(params): Parameters<UpdatePlansParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| characters::update_plans(conn, params))
    }

    #[tool(
        name = "character.change_role",
        description = "Change a character's role (player/companion/friendly/enemy/neutral). Records the pivot in the event log with before/after + narrative reason."
    )]
    async fn character_change_role(
        &self,
        Parameters(params): Parameters<ChangeRoleParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| characters::change_role(conn, params))
    }

    // ── Effects ───────────────────────────────────────────────────────────────

    #[tool(
        name = "apply_effect",
        description = "Apply a temporary numerical modifier to a character. Never mutates base stats — stored as a row consumed by effective-stat composition. Emits effect.applied."
    )]
    async fn apply_effect(
        &self,
        Parameters(params): Parameters<EffectApplyParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| effects::apply(conn, params))
    }

    #[tool(
        name = "dispel_effect",
        description = "Deactivate an active effect (e.g. potion wears off, curse lifted). Emits effect.expired with expiry_reason=dispelled."
    )]
    async fn dispel_effect(
        &self,
        Parameters(params): Parameters<DispelParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| effects::dispel(conn, params))
    }

    // ── Proficiencies ─────────────────────────────────────────────────────────

    #[tool(
        name = "proficiency.set",
        description = "Upsert a character proficiency (skill, save like 'save:con', weapon, tool, or custom growth skill like 'bite'). Fields: name, proficient, expertise (doubles prof bonus), ranks (flat additive)."
    )]
    async fn proficiency_set(
        &self,
        Parameters(params): Parameters<SetProficiencyParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| {
            proficiencies::set_proficiency(conn, params)
        })
    }

    #[tool(
        name = "proficiency.remove",
        description = "Remove a character's proficiency row. Idempotent."
    )]
    async fn proficiency_remove(
        &self,
        Parameters(params): Parameters<RemoveProficiencyParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| {
            proficiencies::remove_proficiency(conn, params)
        })
    }

    // ── Resources ─────────────────────────────────────────────────────────────

    #[tool(
        name = "resource.set",
        description = "Upsert a character resource (spell slot, mana, ki, hit_die, etc). Name is namespaced (e.g. 'slot:1'..'slot:9'). Recharge ∈ short_rest|long_rest|dawn|never|manual."
    )]
    async fn resource_set(
        &self,
        Parameters(params): Parameters<SetResourceParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| proficiencies::set_resource(conn, params))
    }

    #[tool(
        name = "resource.adjust",
        description = "Change a character resource's current value by a signed delta. Clamped to [0, max]. Requires the resource to exist."
    )]
    async fn resource_adjust(
        &self,
        Parameters(params): Parameters<AdjustResourceParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| {
            proficiencies::adjust_resource(conn, params)
        })
    }

    #[tool(
        name = "resource.remove",
        description = "Remove a resource row entirely. Idempotent."
    )]
    async fn resource_remove(
        &self,
        Parameters(params): Parameters<RemoveResourceParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| {
            proficiencies::remove_resource(conn, params)
        })
    }

    // ── Conditions ────────────────────────────────────────────────────────────

    #[tool(
        name = "condition.apply",
        description = "Apply a named condition (blinded, poisoned, mortally_wounded, ...) to a character. Conditions are separate from effects: they carry rule-level riders (disadvantage, auto-fail, speed penalties) defined in content/rules/conditions.yaml."
    )]
    async fn condition_apply(
        &self,
        Parameters(params): Parameters<ApplyConditionParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        with_db_mut(&self.db, move |conn| {
            conditions::apply(conn, &content, params)
        })
    }

    #[tool(
        name = "condition.remove",
        description = "Deactivate an active condition. Emits condition.expired with a reason (save succeeded, spell dispelled, time expired)."
    )]
    async fn condition_remove(
        &self,
        Parameters(params): Parameters<RemoveConditionParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| conditions::remove(conn, params))
    }

    // ── Check resolution ──────────────────────────────────────────────────────

    #[tool(
        name = "resolve_check",
        description = "Resolve a skill / save / attack / ability check. Composes base + effective ability + proficiency + ranks + active effects (flat modifiers and per-check dice like Bless) + condition riders (advantage/disadvantage/auto_fail) + caller-supplied named modifiers. Rolls d20 (or 2d20 for adv/dis) and returns the full breakdown plus success against any DC."
    )]
    async fn resolve_check(
        &self,
        Parameters(params): Parameters<ResolveCheckParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            checks::resolve(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    // ── Campaign setup (Phase 6) ──────────────────────────────────────────────

    #[tool(
        name = "setup.new_campaign",
        description = "Confirm the campaign is in the setup phase and return the bootstrap questions to ask the player. The DB schema is created at server startup; this tool is the protocol-level entry point into the setup flow."
    )]
    async fn setup_new_campaign(
        &self,
        Parameters(_): Parameters<NewCampaignParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            setup::new_campaign(&conn, &content)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "setup.answer",
        description = "Record the player's answer to one setup question. The answer is JSON — a string for single-choice, an array of strings for multi-select. Re-answering the same question id overwrites the previous value."
    )]
    async fn setup_answer(
        &self,
        Parameters(params): Parameters<SetupAnswerParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            setup::answer(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "setup.generate_world",
        description = "Generate the starting zone and 2-5 stub neighbours from the recorded answers. Phase 6 reads the starting_biome answer; later phases will use enemy_preference, tone, etc. Emits a world.generated event. Requires starting_biome to have been answered."
    )]
    async fn setup_generate_world(
        &self,
        Parameters(_): Parameters<GenerateWorldParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            setup::generate_world(&mut conn, &content)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "setup.mark_ready",
        description = "Flip the campaign from 'setup' to 'running'. Records the wall-clock moment as `started_at` on the campaign_state singleton and emits a campaign.started event. Optionally records the player character id for later 'who is the player?' lookups."
    )]
    async fn setup_mark_ready(
        &self,
        Parameters(params): Parameters<MarkReadyParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| setup::mark_ready(conn, params))
    }

    // ── World (Phase 7) ───────────────────────────────────────────────────────

    #[tool(
        name = "world.travel",
        description = "Move a character along an existing zone connection. Advances campaign_hour by the connection's travel_time_hours, updates current_zone_id, upserts character_zone_knowledge to 'visited', stub-generates the destination's missing neighbours on first contact, full-generates landmarks on first visit, and emits location.move."
    )]
    async fn world_travel(
        &self,
        Parameters(params): Parameters<TravelParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| world::travel(conn, params))
    }

    #[tool(
        name = "world.map",
        description = "Return a fog-filtered map rooted at the character's current zone. Walks zone_connections BFS from the origin assigning 2D positions from direction_from. Includes only zones the character has at least 'rumored' knowledge of."
    )]
    async fn world_map(
        &self,
        Parameters(params): Parameters<MapParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db(&self.db, |conn| world::map(conn, params))
    }

    #[tool(
        name = "world.describe_zone",
        description = "Detailed readout of a single zone (name, biome, kind, description, landmarks, outgoing connections), gated by the character's knowledge level. Refuses zones the character has no rumored-or-better knowledge of."
    )]
    async fn world_describe_zone(
        &self,
        Parameters(params): Parameters<DescribeZoneParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db(&self.db, |conn| world::describe_zone(conn, params))
    }

    // ── NPCs (Phase 8) ────────────────────────────────────────────────────────

    #[tool(
        name = "npc.generate",
        description = "Create an NPC from a bundled archetype. Rolls stats within the archetype's ranges, derives HP from its formula, picks a name from the species name pool, inserts proficiencies, rolls the loadout (creating held items), and synthesizes 3-5 history.backstory events at negative campaign_hour. Single transaction."
    )]
    async fn npc_generate(
        &self,
        Parameters(params): Parameters<NpcGenerateParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            npcs::generate(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "character.recall",
        description = "Return events this character participated in (as actor, target, witness, or beneficiary), newest-first. Optional filters: zone_id, other_character_id (co-participation), other_item_id (event references that item), kind_prefix (e.g. 'history.' for backstory only), since_hour (inclusive lower bound; use negative values to include pre-campaign backstory), limit (default 50, max 500)."
    )]
    async fn character_recall(
        &self,
        Parameters(params): Parameters<NpcRecallParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db(&self.db, |conn| npcs::recall(conn, params))
    }

    // ── Encounters (Phase 9) ──────────────────────────────────────────────────

    #[tool(
        name = "encounter.create",
        description = "Create an encounter with participants and an XP budget. Sides: player_side | hostile | neutral | ally. Returns the encounter_id + event_id."
    )]
    async fn encounter_create(
        &self,
        Parameters(params): Parameters<EncCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| encounters::create(conn, params))
    }

    #[tool(
        name = "encounter.complete",
        description = "Mark an encounter's goal as completed. Awards xp_budget * xp_modifier (default 1.0) split across player_side participants and emits encounter.goal_completed. The path is free-text — combat_victory / parley_to_peace / flight / side_swap / redirection / …"
    )]
    async fn encounter_complete(
        &self,
        Parameters(params): Parameters<EncCompleteParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| encounters::complete(conn, params))
    }

    #[tool(
        name = "encounter.abandon",
        description = "Mark an encounter as abandoned (no XP). Emits encounter.abandoned."
    )]
    async fn encounter_abandon(
        &self,
        Parameters(params): Parameters<EncAbandonParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| encounters::abandon(conn, params))
    }

    #[tool(
        name = "encounter.fail",
        description = "Mark an encounter as failed (no XP). Emits encounter.failed."
    )]
    async fn encounter_fail(
        &self,
        Parameters(params): Parameters<EncFailParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| encounters::fail(conn, params))
    }

    // ── Combat (Phase 9) ──────────────────────────────────────────────────────

    #[tool(
        name = "combat.start",
        description = "Enter combat mode on an encounter. Rolls initiative for every participant (d20 + initiative_bonus), sets in_combat=1, round=1, turn_index=0. FIRST auto-ends any other encounter currently in combat, emitting combat.auto_ended on it."
    )]
    async fn combat_start(
        &self,
        Parameters(params): Parameters<CombatStartParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| combat::start(conn, params))
    }

    #[tool(
        name = "combat.next_turn",
        description = "Advance the initiative pointer. At a round boundary, decrements expires_after_rounds on every active effect / condition attached to a participant and deactivates any hitting zero (emits effect.expired / condition.expired)."
    )]
    async fn combat_next_turn(
        &self,
        Parameters(params): Parameters<CombatNextTurnParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| combat::next_turn(conn, params))
    }

    #[tool(
        name = "combat.end",
        description = "Leave combat mode on an encounter. Zeroes out the combat-only participant fields but keeps participant rows (participants outlast combat — see docs/encounters.md)."
    )]
    async fn combat_end(
        &self,
        Parameters(params): Parameters<CombatEndParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| combat::end(conn, params))
    }

    #[tool(
        name = "combat.apply_damage",
        description = "Reduce a character's hp_current by `amount` (non-negative). If HP hits 0 on a previously-alive character, applies mortally_wounded and sets status='unconscious'. Optional damage_type + source are recorded on the event."
    )]
    async fn combat_apply_damage(
        &self,
        Parameters(params): Parameters<ApplyDamageParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| combat::apply_damage(conn, params))
    }

    #[tool(
        name = "combat.apply_healing",
        description = "Increase a character's hp_current by `amount` (non-negative), capped at hp_max. If the character is mortally_wounded and healed above 0 HP, clears the condition and resets death-save counters."
    )]
    async fn combat_apply_healing(
        &self,
        Parameters(params): Parameters<ApplyHealingParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| combat::apply_healing(conn, params))
    }

    #[tool(
        name = "roll_death_save",
        description = "Roll a d20 death save for an unconscious character. ≥10 success; <10 failure; nat 1 counts as two failures; nat 20 auto-stabilises (status='alive', hp=1, counters reset). Three successes → stabilised; three failures → status='dead'."
    )]
    async fn roll_death_save(
        &self,
        Parameters(params): Parameters<DeathSaveParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| combat::roll_death_save(conn, params))
    }

    #[tool(
        name = "roll_death_event",
        description = "Roll the weighted death-events table from content/rules/death_events.yaml. Requires status='dead'. Returns the chosen event kind + description + outcome_hooks so the DM agent can narrate the aftermath (bargain, ghost-for-a-time, resurrection, etc.)."
    )]
    async fn roll_death_event(
        &self,
        Parameters(params): Parameters<DeathEventParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            combat::roll_death_event(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    // ── Rests (Phase 9) ───────────────────────────────────────────────────────

    #[tool(
        name = "rest.short",
        description = "Short rest: refills resources with recharge='short_rest'. Does not restore HP."
    )]
    async fn rest_short(
        &self,
        Parameters(params): Parameters<ShortRestParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| rests::short_rest(conn, params))
    }

    #[tool(
        name = "rest.long",
        description = "Long rest: refills resources with recharge ∈ {short_rest, long_rest, dawn}; restores HP to hp_max; clears death-save counters if alive. Refuses if the character is dead."
    )]
    async fn rest_long(
        &self,
        Parameters(params): Parameters<LongRestParams>,
    ) -> Result<CallToolResult, McpError> {
        with_db_mut(&self.db, |conn| rests::long_rest(conn, params))
    }

    // ── Inventory (Phase 10) ──────────────────────────────────────────────────

    #[tool(
        name = "inventory.create",
        description = "Create a new item instance. Exactly one of holder_character_id / zone_location_id / container_item_id must be set. base_kind must match a bundled content item base. Stackables accept quantity > 1."
    )]
    async fn inventory_create(
        &self,
        Parameters(params): Parameters<InvCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            inventory::create(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "inventory.pickup",
        description = "Character picks up a zone-located item. Refuses if carried weight would exceed overloaded_threshold_pct (default 100%) — returns {error:'would_overload', ...}. Applies the 'encumbered' condition if carried weight crosses encumbered_threshold_pct (default 67%)."
    )]
    async fn inventory_pickup(
        &self,
        Parameters(params): Parameters<PickupParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            inventory::pickup(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        // Returns a Result<PickupResult, PickupRefused> — serialize either side.
        let json = match value {
            Ok(ok) => serde_json::to_string(&ok),
            Err(refused) => serde_json::to_string(&refused),
        }
        .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "inventory.drop",
        description = "Character drops a held item into their current zone. Re-evaluates encumbrance; crossing back below encumbered_threshold_pct clears the 'encumbered' condition."
    )]
    async fn inventory_drop(
        &self,
        Parameters(params): Parameters<DropParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            inventory::drop_item(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "inventory.equip",
        description = "Set equipped_slot on a held item (main-hand, off-hand, head, chest, …). Requires the character to hold the item."
    )]
    async fn inventory_equip(
        &self,
        Parameters(params): Parameters<EquipParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            inventory::equip(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "inventory.unequip",
        description = "Clear equipped_slot on a held item."
    )]
    async fn inventory_unequip(
        &self,
        Parameters(params): Parameters<UnequipParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            inventory::unequip(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "inventory.get",
        description = "Full inventory readout for a character: every held item with effective_weight_lb and effective_value_gp, plus carried_weight_lb / capacity_lb / percent_of_capacity / encumbered flag."
    )]
    async fn inventory_get(
        &self,
        Parameters(InvGetParams { character_id }): Parameters<InvGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            inventory::get(&conn, &content, character_id)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "inventory.inspect",
        description = "Effective stats of a single item (weight, value) composed from its base kind + quantity."
    )]
    async fn inventory_inspect(
        &self,
        Parameters(InvInspectParams { item_id }): Parameters<InvInspectParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            inventory::inspect(&conn, &content, item_id)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "inventory.transfer",
        description = "Move an item to a new location. Exactly one of to_character_id / to_container_item_id / to_zone_location_id must be set. Does not apply encumbrance checks — prefer inventory.pickup for gear the character is picking up."
    )]
    async fn inventory_transfer(
        &self,
        Parameters(params): Parameters<InvTransferParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            inventory::transfer(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        name = "barter.exchange",
        description = "Barter items (and gold) between the player and a merchant. Auto-accepts offers at or above 90% of requested value; refuses manifestly bad deals (below 50%); otherwise rolls a persuasion check against a DC derived from the value gap. On success, both inventories swap atomically. dc_override available for DM-authored trades."
    )]
    async fn barter_exchange(
        &self,
        Parameters(params): Parameters<BarterExchangeParams>,
    ) -> Result<CallToolResult, McpError> {
        let content = Arc::clone(&self.content);
        let value = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| McpError::internal_error("DB mutex poisoned".to_string(), None))?;
            barter::exchange(&mut conn, &content, params)
                .map_err(|e| McpError::invalid_params(format!("{e:#}"), None))?
        };
        let json = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(format!("serialise response: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for DmMcpHandler {
    fn get_info(&self) -> ServerInfo {
        let mut implementation = rmcp::model::Implementation::default();
        implementation.name = env!("CARGO_PKG_NAME").to_string();
        implementation.version = env!("CARGO_PKG_VERSION").to_string();

        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = implementation;
        info.instructions = Some(
            "dm-mcp: MCP toolkit for AI Dungeon Masters. Phase 10 adds inventory (create/pickup/drop/equip/unequip/get/inspect/transfer) with encumbrance enforcement (pickup refused above overloaded threshold, 'encumbered' condition applied between encumbered and overloaded thresholds) and barter.exchange (persuasion-check-driven rate). Live tools: server.info, content.introspect, dice.roll, character.*, apply_effect, dispel_effect, proficiency.*, resource.*, condition.*, resolve_check, setup.*, world.*, npc.generate, character.recall, encounter.*, combat.*, roll_death_save, roll_death_event, rest.short, rest.long, inventory.*, barter.exchange.".to_string(),
        );
        info
    }
}
