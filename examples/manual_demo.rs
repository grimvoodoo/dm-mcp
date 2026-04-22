//! Manual-test walkthrough for dm-mcp over stdio.
//!
//! Spawns the compiled binary as an MCP child process, completes the handshake, lists the
//! registered tools, calls `server.info`, pretty-prints every response the client sees, and
//! shuts the session down cleanly.
//!
//! This is the "see it respond to requests" counterpart to the automated integration tests
//! in `tests/`. The same rmcp client library that a real DM agent uses is what drives this
//! demo, so what prints here is exactly what an agent would receive on the wire.
//!
//! ## Run
//!
//! ```sh
//! make demo
//! ```
//!
//! Or manually:
//!
//! ```sh
//! cargo build && cargo run --example manual_demo
//! ```
//!
//! ## Seeing the wire traffic
//!
//! rmcp emits `trace`-level log events for every JSON-RPC frame it sends and receives.
//! To see them, set `RUST_LOG`:
//!
//! ```sh
//! RUST_LOG=rmcp=trace make demo
//! ```
//!
//! Server-side logs write to stderr so they interleave visibly with the client-side output.

use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tokio::process::Command;

fn banner(n: u32, title: &str) {
    println!();
    println!("────────────────────────────────────────────────────────────");
    println!(" {n}. {title}");
    println!("────────────────────────────────────────────────────────────");
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install a simple tracing subscriber so RUST_LOG=rmcp=trace shows wire frames on stderr.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    // Examples don't get `CARGO_BIN_EXE_<name>`, so locate the dm-mcp binary relative to
    // where *this* example was built. current_exe() is target/<profile>/examples/manual_demo,
    // so walking up twice lands in target/<profile>/ — which holds the sibling dm-mcp binary
    // for the same profile. This honors CARGO_TARGET_DIR and works for both debug and release.
    // `make demo` depends on `_build-debug`, which guarantees dm-mcp exists in that directory.
    let current_exe = std::env::current_exe().context("locate manual_demo executable")?;
    let profile_dir = current_exe
        .parent()
        .and_then(|examples_dir| examples_dir.parent())
        .context("manual_demo should run from target/<profile>/examples")?;
    let bin_path = profile_dir.join(format!("dm-mcp{}", std::env::consts::EXE_SUFFIX));
    if !bin_path.exists() {
        anyhow::bail!(
            "dm-mcp binary not found at {}.\n  \
             Run `cargo build` first, or use `make demo` which builds + runs in one step.",
            bin_path.display()
        );
    }

    banner(1, "Spawning dm-mcp as an MCP stdio child process");
    println!("  binary : {}", bin_path.display());
    println!("  args   : stdio");
    println!("  env    : DMMCP_LOG_LEVEL=warn");
    let mut cmd = Command::new(&bin_path);
    cmd.arg("stdio");
    cmd.env("DMMCP_LOG_LEVEL", "warn");
    let transport = TokioChildProcess::new(cmd).context("spawn child")?;

    banner(2, "Completing the MCP handshake (initialize → initialized)");
    let client = ().serve(transport).await.context("mcp handshake")?;
    let info = client
        .peer_info()
        .context("peer_info should be populated after handshake")?;
    println!("  server name      : {}", info.server_info.name);
    println!("  server version   : {}", info.server_info.version);
    println!("  protocol version : {:?}", info.protocol_version);
    if let Some(inst) = &info.instructions {
        println!("  instructions     : {inst}");
    }

    banner(3, "Listing available tools (tools/list RPC)");
    let tools = client.list_all_tools().await.context("list_all_tools")?;
    if tools.is_empty() {
        println!("  (no tools registered)");
    } else {
        for tool in &tools {
            println!("  • {name}", name = tool.name,);
            if let Some(desc) = tool.description.as_deref() {
                println!("      {desc}");
            }
        }
        println!();
        println!("  {} total", tools.len());
    }

    banner(4, "Calling tool: server.info (tools/call RPC)");
    let result = client
        .call_tool(CallToolRequestParams::new("server.info"))
        .await
        .context("call_tool server.info")?;
    // Per the MCP spec `is_error` is optional — `None` and `Some(false)` both mean
    // "success", only `Some(true)` is an actual error. Collapse to a bool for display.
    let is_error = result.is_error.unwrap_or(false);
    println!("  is_error : {is_error}");
    if result.content.is_empty() {
        println!("  content  : <empty>");
    }
    for item in &result.content {
        match &item.raw {
            RawContent::Text(t) => {
                println!("  text     : {}", t.text);
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t.text) {
                    if let Some(obj) = v.as_object() {
                        println!("  parsed   :");
                        for (k, val) in obj {
                            println!("    {k} = {val}");
                        }
                    }
                }
            }
            other => println!("  non-text : {other:?}"),
        }
    }

    banner(5, "Cancelling the session cleanly");
    client.cancel().await.context("cancel client")?;
    println!("  ✓ session closed");
    println!();

    Ok(())
}
