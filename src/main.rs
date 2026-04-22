//! dm-mcp — MCP toolkit for AI Dungeon Masters running solo d20-inspired RPG campaigns.
//!
//! CLI entry point. Loads config from env vars, opens the campaign database (applying every
//! PRAGMA), parses bundled YAML content into memory, then dispatches to the chosen transport.
//! See [`docs/architecture.md`](../docs/architecture.md) for design rationale.

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

mod characters;
mod checks;
mod conditions;
mod config;
mod content;
mod db;
mod dice;
mod effects;
mod events;
mod handler;
mod proficiencies;
mod setup;
mod transport;
mod world;

use crate::config::Config;
use crate::content::Content;

/// dm-mcp — MCP toolkit for AI Dungeon Masters running solo d20-inspired RPG campaigns.
#[derive(Parser, Debug)]
#[command(
    name = "dm-mcp",
    version,
    about = "MCP toolkit for AI Dungeon Masters running solo d20-inspired RPG campaigns"
)]
struct Cli {
    #[command(subcommand)]
    transport: TransportCmd,
}

#[derive(Subcommand, Debug)]
enum TransportCmd {
    /// Serve MCP over stdin/stdout (lowest latency; for local DM agents).
    Stdio,
    /// Serve MCP over streamable HTTP (for Kubernetes / networked deploys). Exposes /healthz.
    Http,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    init_tracing(&cli.transport, &cfg.log_level)?;

    // Load bundled (or overridden) content once. Held in an Arc so both transports can
    // share a single catalog across MCP sessions.
    let content = Arc::new(Content::load(cfg.content_dir.as_deref())?);
    tracing::info!(
        abilities = content.abilities.len(),
        skills = content.skills.len(),
        damage_types = content.damage_types.len(),
        conditions = content.conditions.len(),
        biomes = content.biomes.len(),
        weapons = content.weapons.len(),
        enchantments = content.enchantments.len(),
        archetypes = content.archetypes.len(),
        "dm-mcp: content catalog loaded"
    );

    // Open the campaign database — applies PRAGMAs and runs migrations. The handle is
    // shared across MCP sessions so tools can read/write the single campaign connection.
    let db = db::open(&cfg.db)?;
    tracing::info!(path = %cfg.db.path.display(), "dm-mcp: campaign database opened");

    match cli.transport {
        TransportCmd::Stdio => transport::stdio::run(content, db).await,
        TransportCmd::Http => transport::http::run(&cfg.http, content, db).await,
    }
}

/// Initialise tracing. stdio mode writes logs to stderr so the stdout channel stays reserved
/// for the MCP protocol frames. HTTP mode also writes to stderr for operational consistency.
fn init_tracing(_transport: &TransportCmd, level: &str) -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .map_err(|e| anyhow::anyhow!("failed to init tracing: {e}"))?;
    Ok(())
}
