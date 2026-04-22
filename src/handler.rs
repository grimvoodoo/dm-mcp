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

use crate::content::Content as ContentCatalog;
use crate::dice;

/// Arguments for the `dice.roll` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiceRollParams {
    /// Dice notation. Accepted shapes: `d4`/`d6`/`d8`/`d10`/`d12`/`d20`/`d100` (single die),
    /// `3d6` (count × sides), or `11-43` (inclusive integer range).
    pub spec: String,
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
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl DmMcpHandler {
    pub fn new(transport: Transport, content: Arc<ContentCatalog>) -> Self {
        Self {
            transport,
            content,
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
            "dm-mcp: MCP toolkit for AI Dungeon Masters. Phase 3 adds dice rolling. Live tools: server.info, content.introspect, dice.roll.".to_string(),
        );
        info
    }
}
