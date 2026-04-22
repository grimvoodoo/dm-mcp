//! E2E test for Phase 2: spawn the stdio transport, call `content.introspect`, assert the
//! response contains the expected content IDs for every loaded section.

use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tempfile::TempDir;
use tokio::process::Command;

fn bin_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_dm-mcp"))
}

async fn connect_with_db(
    db_path: &std::path::Path,
) -> Result<rmcp::service::RunningService<rmcp::service::RoleClient, ()>> {
    let mut cmd = Command::new(bin_path());
    cmd.arg("stdio");
    cmd.env("DMMCP_LOG_LEVEL", "warn");
    cmd.env("DMMCP_DB_PATH", db_path);
    let transport = TokioChildProcess::new(cmd).context("spawn child")?;
    let client = ().serve(transport).await.context("mcp handshake")?;
    Ok(client)
}

#[tokio::test]
async fn content_introspect_returns_every_section() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("campaign.db");
    let client = connect_with_db(&db_path).await?;

    // Tool must appear in list_tools.
    let tools = client.list_all_tools().await?;
    assert!(
        tools.iter().any(|t| t.name == "content.introspect"),
        "content.introspect should be listed; got {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    // Call it and parse the JSON payload.
    let result = client
        .call_tool(CallToolRequestParams::new("content.introspect"))
        .await
        .context("call content.introspect")?;
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
        .context("expected text content in response")?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("introspect payload should be JSON, got: {text}"))?;

    // Expected section -> at least one ID it must contain.
    let expectations: &[(&str, &[&str])] = &[
        ("abilities", &["str", "cha", "con"]),
        ("skills", &["stealth", "persuasion", "perception"]),
        ("damage_types", &["fire", "slashing", "necrotic"]),
        ("conditions", &["blinded", "paralyzed", "mortally_wounded"]),
        ("biomes", &["temperate_forest"]),
        ("weapons", &["longsword"]),
        ("enchantments", &["glowing"]),
        ("archetypes", &["village_elder"]),
    ];
    for (section, required_ids) in expectations {
        let arr = parsed
            .get(*section)
            .and_then(|v| v.as_array())
            .with_context(|| {
                format!("section {section} should be an array; payload was {parsed}")
            })?;
        let ids: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        for required in *required_ids {
            assert!(
                ids.iter().any(|id| id == required),
                "section {section} should contain {required}; got {ids:?}"
            );
        }
    }

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn server_info_still_works_alongside_new_tool() -> Result<()> {
    // Phase 1's smoke test still needs to pass after adding new tools — make sure we haven't
    // regressed the router by adding content.introspect.
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("campaign.db");
    let client = connect_with_db(&db_path).await?;

    let result = client
        .call_tool(CallToolRequestParams::new("server.info"))
        .await?;
    assert!(result.is_error != Some(true));
    let text = result
        .content
        .iter()
        .find_map(|item| match &item.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .context("expected text content")?;
    let parsed: serde_json::Value = serde_json::from_str(&text)?;
    assert_eq!(parsed["name"], "dm-mcp");
    assert_eq!(parsed["transport"], "stdio");

    client.cancel().await?;
    Ok(())
}
