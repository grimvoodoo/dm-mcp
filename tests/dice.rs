//! E2E test for Phase 3: `dice.roll` over stdio MCP covering the three notation shapes
//! the Roadmap row specifies.

use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tempfile::TempDir;
use tokio::process::Command;

fn bin_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_dm-mcp"))
}

async fn connect(
    tmp: &TempDir,
) -> Result<rmcp::service::RunningService<rmcp::service::RoleClient, ()>> {
    let mut cmd = Command::new(bin_path());
    cmd.arg("stdio");
    cmd.env("DMMCP_LOG_LEVEL", "warn");
    cmd.env("DMMCP_DB_PATH", tmp.path().join("campaign.db"));
    let transport = TokioChildProcess::new(cmd).context("spawn child")?;
    let client = ().serve(transport).await.context("mcp handshake")?;
    Ok(client)
}

/// Call `dice.roll` with `spec`, parse the JSON payload, hand back the decoded value.
async fn call_roll(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    spec: &str,
) -> Result<serde_json::Value> {
    let args = serde_json::json!({ "spec": spec });
    let params =
        CallToolRequestParams::new("dice.roll").with_arguments(args.as_object().unwrap().clone());
    let result = client
        .call_tool(params)
        .await
        .with_context(|| format!("dice.roll({spec:?})"))?;
    assert!(
        result.is_error != Some(true),
        "dice.roll({spec:?}) signalled error: {:?}",
        result
    );
    let text = result
        .content
        .iter()
        .find_map(|item| match &item.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .context("expected a text content block in dice.roll result")?;
    serde_json::from_str(&text)
        .with_context(|| format!("dice.roll({spec:?}) payload should be JSON, got: {text}"))
}

#[tokio::test]
async fn dice_roll_tool_is_listed() -> Result<()> {
    let tmp = TempDir::new()?;
    let client = connect(&tmp).await?;
    let tools = client.list_all_tools().await?;
    assert!(
        tools.iter().any(|t| t.name == "dice.roll"),
        "dice.roll should be listed; got {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn d20_result_is_in_range() -> Result<()> {
    // Roadmap assertion: `dice.roll("d20")` → result ∈ [1, 20].
    let tmp = TempDir::new()?;
    let client = connect(&tmp).await?;

    for _ in 0..20 {
        let v = call_roll(&client, "d20").await?;
        let total = v["total"].as_i64().context("total should be int")?;
        assert!(
            (1..=20).contains(&total),
            "d20 total {total} out of [1, 20]; payload was {v}"
        );
        let rolls = v["rolls"].as_array().context("rolls should be array")?;
        assert_eq!(rolls.len(), 1, "d20 should have exactly one roll");
        assert_eq!(rolls[0].as_i64(), Some(total));
    }

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn three_d6_has_three_rolls_summing_to_total() -> Result<()> {
    // Roadmap assertion: `dice.roll("3d6")` → 3 rolls, sum equals reported total.
    let tmp = TempDir::new()?;
    let client = connect(&tmp).await?;

    for _ in 0..20 {
        let v = call_roll(&client, "3d6").await?;
        let total = v["total"].as_i64().context("total")?;
        let rolls = v["rolls"].as_array().context("rolls")?;
        assert_eq!(rolls.len(), 3, "3d6 should have three rolls; got {v}");
        let sum: i64 = rolls.iter().map(|r| r.as_i64().unwrap_or(0)).sum();
        assert_eq!(
            total, sum,
            "sum-of-rolls should equal total; payload was {v}"
        );
        for r in rolls {
            let x = r.as_i64().unwrap();
            assert!((1..=6).contains(&x), "3d6 roll {x} out of [1, 6]");
        }
    }

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn range_result_is_in_inclusive_bounds() -> Result<()> {
    // Roadmap assertion: `dice.roll("11-43")` → result ∈ [11, 43].
    let tmp = TempDir::new()?;
    let client = connect(&tmp).await?;

    for _ in 0..50 {
        let v = call_roll(&client, "11-43").await?;
        let total = v["total"].as_i64().context("total")?;
        assert!(
            (11..=43).contains(&total),
            "range total {total} out of [11, 43]; payload was {v}"
        );
    }

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_spec_surfaces_as_tool_error() -> Result<()> {
    let tmp = TempDir::new()?;
    let client = connect(&tmp).await?;

    // An obviously-bad spec should produce an MCP error, not a panic and not a silent
    // zero-result. call_tool should return Err (which rmcp turns into an error
    // CallToolResult via invalid_params), OR the result carries is_error=true.
    let args = serde_json::json!({ "spec": "not-a-dice-spec" });
    let params =
        CallToolRequestParams::new("dice.roll").with_arguments(args.as_object().unwrap().clone());
    let outcome = client.call_tool(params).await;
    match outcome {
        Err(_) => { /* JSON-RPC error — fine */ }
        Ok(result) => {
            assert_eq!(
                result.is_error,
                Some(true),
                "bogus spec should set is_error=true; got {result:?}"
            );
        }
    }

    client.cancel().await?;
    Ok(())
}
