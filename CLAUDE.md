# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- Build: `cargo build` (or `cargo build --release`)
- Run (stdio transport): `cargo run stdio`
- Run (http transport, port 3000): `cargo run httpstream`
- Tests (all): `cargo test`
- Single test: `cargo test <name>` — e.g. `cargo test test_roll_dice_custom_range`

The binary requires a transport argument (`stdio` or `httpstream`); it will exit with a usage message otherwise.

## Architecture

Small Rust binary. `src/main.rs` parses `argv[1]` and dispatches to one of two transport modules. All dice logic lives in `src/dice.rs` and is transport-agnostic; transports call into it and serialize `RollResult` as JSON.

- `dice.rs` — core logic. `roll_dice(&str)` accepts three input shapes: `"d6"`-style single die, `"3d6"`-style multi-dice (returns per-die `results` plus summed `result`), and `"11-52"`-style custom inclusive range. `roll_multiple_dice_request` is the explicit multi-dice API. Unknown dice strings silently default to `d20` — `parse_dice_type` returns `u32` (not `Result`), so invalid input produces a valid roll rather than an error.
- `stdio.rs` — line-buffered loop over stdin. **Duplicates multi-dice parsing** that already exists in `dice.rs`: it splits on `'d'` itself and calls `roll_multiple_dice_request` directly, rather than letting `roll_dice` handle the `"NdM"` case. Keep this in mind when changing the multi-dice parser — there are two call sites.
- `httpstream.rs` — **stub, not a working HTTP server.** Despite the README's API documentation, this function only prints a placeholder message and returns. The hyper/rustls dependencies are declared but unused. Implementing the real HTTP server is an open task, not a maintained feature.

## Gotchas

- **`rust-mcp-sdk` / `rust-mcp-transport` in `Cargo.toml` are unused.** Nothing in `src/` imports them. The project is named "MCP server" but currently speaks plain newline-delimited JSON on stdio — it is not an MCP protocol implementation. Re-adding real MCP support is likely on the roadmap; don't assume existing scaffolding is in place.
- **`src/dice_test.rs` is dead code.** It is not declared as a module in `main.rs` and its contents reference an older API (`parse_dice_type` returning `Result`, a `RollRequest` with `die_type`/`min_value`/`max_value` fields) that no longer exists. The live tests are the `#[cfg(test)] mod tests` block at the bottom of `dice.rs`. Don't trust `dice_test.rs` as a spec — if you wire it up, it won't compile.
- `roll_dice` returns `results: None` for single-die and custom-range inputs, and `results: Some(vec)` only for multi-dice. Callers must handle both.
