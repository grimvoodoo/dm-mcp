//! Stdio MCP transport.
//!
//! The lowest-latency transport: attach the MCP server directly to the process's stdin/stdout.
//! Chosen for local DM-agent setups. See `docs/architecture.md`.

use anyhow::Result;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

use crate::handler::{DmMcpHandler, Transport};

/// Run the MCP server over stdin/stdout until the peer closes the connection.
pub async fn run() -> Result<()> {
    tracing::info!("dm-mcp: serving MCP over stdio");
    let handler = DmMcpHandler::new(Transport::Stdio);
    let service = handler.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
