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

mod barter;
mod characters;
mod checks;
mod combat;
mod conditions;
mod config;
mod content;
mod db;
mod dice;
mod effects;
mod encounters;
mod events;
mod handler;
mod inventory;
mod npcs;
mod proficiencies;
mod rests;
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
    /// Probe the HTTP transport's `/healthz` endpoint and exit 0 on 200, non-zero otherwise.
    /// Used by `HEALTHCHECK` in the container image — scratch has no shell, so a binary
    /// subcommand is the cheapest way to wire one without pulling in a base image with
    /// curl/wget. Reads `DMMCP_HTTP_BIND` and `DMMCP_HTTP_PORT` from env to find the
    /// running server. Does NOT open the database — purely a network reachability probe.
    Healthcheck,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    // healthcheck is a thin probe — no tracing init (would noise up the container's
    // stdout per probe), no content load, no DB open. Just a TCP + HTTP exit code.
    if matches!(cli.transport, TransportCmd::Healthcheck) {
        return run_healthcheck(&cfg.http).await;
    }

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
        TransportCmd::Healthcheck => unreachable!("handled above"),
    }
}

/// Make a single HTTP/1.1 GET to `/healthz` against the configured bind+port and exit
/// 0 if the server responds 200, non-zero otherwise. Avoids pulling reqwest into the
/// release binary (extra ~1MB) by speaking the bare HTTP/1.1 request format directly
/// over a `tokio::net::TcpStream`. The probe is fast (5s connect timeout) so it's
/// suitable for HEALTHCHECK directives that fire every few seconds.
async fn run_healthcheck(http: &config::HttpConfig) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::{timeout, Duration};

    let addr = http.socket_addr();
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("connect to {addr} timed out"))?
        .map_err(|e| anyhow::anyhow!("connect to {addr}: {e}"))?;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .map_err(|e| anyhow::anyhow!("write request: {e}"))?;
    let mut buf = Vec::with_capacity(256);
    timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("read response from {addr} timed out"))?
        .map_err(|e| anyhow::anyhow!("read response: {e}"))?;
    let response = String::from_utf8_lossy(&buf);
    if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        let preview: String = response.chars().take(120).collect();
        anyhow::bail!("healthz returned non-200: {preview}")
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
