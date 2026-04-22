//! E2E test: MCP handshake over stdio and `server.info` tool invocation.
//!
//! Spawns the compiled binary as an MCP stdio child, uses rmcp's client side to negotiate
//! the protocol, lists tools, calls `server.info`, and asserts the result shape.

use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tempfile::TempDir;
use tokio::process::Command;

fn bin_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_dm-mcp"))
}

/// Spawn the binary with an isolated tmp campaign DB. Tests run in parallel by default,
/// so sharing `./campaign.db` between spawns would race on schema migration.
async fn connect(
    _tmp: &TempDir,
) -> Result<rmcp::service::RunningService<rmcp::service::RoleClient, ()>> {
    let mut cmd = Command::new(bin_path());
    cmd.arg("stdio");
    cmd.env("DMMCP_LOG_LEVEL", "warn");
    cmd.env("DMMCP_DB_PATH", _tmp.path().join("campaign.db"));
    let transport = TokioChildProcess::new(cmd).context("spawn child")?;
    let client = ().serve(transport).await.context("mcp handshake")?;
    Ok(client)
}

#[tokio::test]
async fn handshake_and_peer_info_reports_server_name() -> Result<()> {
    let tmp = TempDir::new()?;
    let client = connect(&tmp).await?;

    let info = client
        .peer_info()
        .context("peer_info should be populated after handshake")?;
    assert_eq!(
        info.server_info.name, "dm-mcp",
        "server name should be the crate name, got {:?}",
        info.server_info.name
    );
    assert!(
        !info.server_info.version.is_empty(),
        "version should be non-empty"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn server_info_tool_is_listed() -> Result<()> {
    let tmp = TempDir::new()?;
    let client = connect(&tmp).await?;

    let tools = client.list_all_tools().await?;
    assert!(
        tools.iter().any(|t| t.name == "server.info"),
        "server.info tool should be listed; got {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn server_info_tool_returns_expected_shape() -> Result<()> {
    let tmp = TempDir::new()?;
    let client = connect(&tmp).await?;

    let result = client
        .call_tool(CallToolRequestParams::new("server.info"))
        .await
        .context("call server.info")?;

    assert!(
        result.is_error != Some(true),
        "tool should not signal error, got {:?}",
        result.is_error
    );

    let text = result
        .content
        .iter()
        .find_map(|item| match &item.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .context("expected a text content block in server.info result")?;

    let parsed: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("server.info payload should be JSON, got: {text}"))?;

    let obj = parsed
        .as_object()
        .context("server.info payload should be a JSON object")?;
    assert_eq!(
        obj.get("name").and_then(|v| v.as_str()),
        Some("dm-mcp"),
        "name field mismatch"
    );
    assert!(
        obj.get("version")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "version field should be a non-empty string"
    );
    assert_eq!(
        obj.get("transport").and_then(|v| v.as_str()),
        Some("stdio"),
        "transport field should report 'stdio' when invoked over stdio"
    );

    client.cancel().await?;
    Ok(())
}
