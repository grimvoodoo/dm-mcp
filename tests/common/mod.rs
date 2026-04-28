//! Shared test harness for MCP integration tests.
//!
//! Cargo's special-cases `tests/common/mod.rs` (and any other path that isn't a top-level
//! `.rs` file directly under `tests/`) as a non-test module, so each integration test
//! file can `mod common;` to pull in the harness without producing a "tests/common.rs
//! has no tests" warning.
//!
//! What lives here:
//!
//! - [`bin_path`] — looks up the compiled `dm-mcp` binary via `CARGO_BIN_EXE_dm-mcp`.
//! - [`Harness`] — bundles a per-test `TempDir` (auto-deleted on drop), the resulting
//!   `db_path`, and the running rmcp client.
//! - [`connect`] — spawns the binary as a stdio MCP child and returns a [`Harness`].
//! - [`call`] — invokes a tool and parses the response text as JSON, asserting the
//!   tool didn't signal `is_error: true`.
//!
//! What does NOT live here:
//!
//! - Per-file helpers like `make_char` (different signatures per test file's needs)
//!   and `setup_world` (per-file orchestration of the campaign-setup flow). Those stay
//!   in their respective test files.
//!
//! Each test that needs the harness adds two lines at the top:
//!
//! ```text
//! mod common;
//! use common::{call, connect, Harness};
//! ```

#![allow(dead_code)] // not every test file uses every helper

use std::path::PathBuf;

use anyhow::{Context, Result};
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tempfile::TempDir;
use tokio::process::Command;

/// Path to the compiled `dm-mcp` binary. Cargo defines `CARGO_BIN_EXE_<name>` for
/// integration tests of binary crates, so no manual lookup is needed.
pub fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dm-mcp"))
}

/// MCP test harness. The `_tmp` TempDir is held to keep the campaign DB on disk for
/// the duration of the test; dropping it on test exit deletes the file.
pub struct Harness {
    pub _tmp: TempDir,
    pub db_path: PathBuf,
    pub client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
}

/// Spawn the `dm-mcp` binary as a stdio MCP child and complete the rmcp handshake.
/// Each call gets its own TempDir-backed campaign DB so tests can run in parallel
/// without racing on schema migration.
pub async fn connect() -> Result<Harness> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("campaign.db");
    let mut cmd = Command::new(bin_path());
    cmd.arg("stdio");
    cmd.env("DMMCP_LOG_LEVEL", "warn");
    cmd.env("DMMCP_DB_PATH", &db_path);
    let transport = TokioChildProcess::new(cmd).context("spawn child")?;
    let client = ().serve(transport).await.context("mcp handshake")?;
    Ok(Harness {
        _tmp: tmp,
        db_path,
        client,
    })
}

/// Invoke a tool with the given JSON args and parse the text payload as JSON. Asserts
/// the tool didn't signal `is_error: true` — tests that expect an error path should
/// call `client.call_tool(...)` directly and inspect the result.
pub async fn call(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    let obj = args.as_object().cloned().unwrap_or_default();
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(obj);
    let result = client
        .call_tool(params)
        .await
        .with_context(|| format!("call {name}"))?;
    assert!(
        result.is_error != Some(true),
        "{name} signalled error: {:?}",
        result
    );
    let text = result
        .content
        .iter()
        .find_map(|item| match &item.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .context("expected text content")?;
    serde_json::from_str(&text).with_context(|| format!("{name} payload: {text}"))
}
