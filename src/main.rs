//! dm-mcp — MCP toolkit for AI Dungeon Masters running solo d20-inspired RPG campaigns.
//!
//! CLI entry point. Dispatches to one of the two transport modules based on the chosen
//! subcommand. See [`docs/architecture.md`](../docs/architecture.md) for design rationale.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

mod config;
mod handler;
mod transport;

use crate::config::Config;

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

    match cli.transport {
        TransportCmd::Stdio => transport::stdio::run().await,
        TransportCmd::Http => transport::http::run(&cfg.http).await,
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
