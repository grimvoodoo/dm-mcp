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

use crate::characters::{
    self, ChangeRoleParams, CreateParams as CharCreateParams, GetParams, UpdatePlansParams,
};
use crate::checks::{self, ResolveCheckParams};
use crate::conditions::{self, ApplyConditionParams, RemoveConditionParams};
use crate::content::Content as ContentCatalog;
use crate::db::DbHandle;
use crate::dice;
use crate::effects::{self, ApplyParams as EffectApplyParams, DispelParams};
use crate::proficiencies::{
    self, AdjustResourceParams, RemoveProficiencyParams, RemoveResourceParams,
    SetProficiencyParams, SetResourceParams,
};
use crate::setup::{
    self, AnswerParams as SetupAnswerParams, GenerateWorldParams, MarkReadyParams,
    NewCampaignParams,
};

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
        with_db_mut(&self.db, |conn| conditions::apply(conn, params))
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
            "dm-mcp: MCP toolkit for AI Dungeon Masters. Phase 4 adds character CRUD with effective-stat composition, effects apply/dispel, proficiencies CRUD, resources CRUD. Live tools: server.info, content.introspect, dice.roll, character.create, character.get, character.update_plans, character.change_role, apply_effect, dispel_effect, proficiency.set, proficiency.remove, resource.set, resource.adjust, resource.remove.".to_string(),
        );
        info
    }
}
