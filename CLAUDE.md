# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`dm-mcp` is an MCP (Model Context Protocol) toolkit for an AI Dungeon Master running solo RPG campaigns. The consumer is always an LLM DM agent — tool responses are structured JSON; narration is the agent's job.

## Status & where to start

**Pre-MVP.** The `main` branch carries a dice-rolling stub; the `feature/re-design` branch holds the full architectural design and the phased implementation plan.

Design is **complete** and documented in [`docs/`](docs/README.md) — organised by area (architecture, event log, characters, checks, items, world, encounters, NPCs, campaign setup, content, IP/licensing). Every decision includes rationale.

Implementation is **in progress** via 10 small, testable phases tracked in the [README.md Roadmap](README.md#roadmap).

**If you're starting a fresh session and don't know what to do:** read the Roadmap, find the next unchecked phase, and follow the per-phase workflow documented below the table.

## Hard rules (non-negotiable)

These affect every coding decision. Do not relax or defer them.

- **TLS via rustls only, never OpenSSL.** The release target is a `scratch` container image. OpenSSL breaks scratch builds. Before adding any dependency that does networking or HTTP, audit its default features and explicitly select a rustls backend (e.g. `rusqlite` features excluding `bundled-sqlcipher`, `reqwest = { features = ["rustls-tls"] }`). If a dep silently pulls `native-tls` or `openssl-sys`, treat it as a build-blocker — swap it or disable default features. Run `cargo tree -i openssl` periodically; non-empty output means something regressed.

- **No WotC trademarks or non-SRD copyrighted text.** See [`docs/ip-and-licensing.md`](docs/ip-and-licensing.md) for the full rule. Never use "D&D", "Dungeons & Dragons", "5e", or "5th Edition" branding in README, docs, code comments, strings, commit messages, or container labels — use "d20-inspired" instead. Content is original or drawn only from the 5.1 SRD (CC-BY-4.0) or ORC-licensed material. No copy-paste of non-SRD product-book text, even mechanically-identical stat blocks.

- **Content is data, not code.** Anything describing "what a thing is" (item definitions, archetype stats, condition riders, biome templates, encounter resolution paths) lives in bundled YAML under `content/`. Only per-instance data lives in SQLite. Before adding a column to support a new rule, ask whether it can live in content instead. Most answers are yes.

- **Latency is a first-class goal.** The MCP sits in the hot path of every player turn. Defaults committed to: SQLite PRAGMAs set at connection open (`journal_mode=WAL`, `synchronous=NORMAL`, `mmap_size`, `cache_size`, `foreign_keys=ON`), prepared-statement caching for every hot query, batched writes per tool call in a single transaction, content parsed once at startup into in-memory structs. Event summaries are code-templated from kind + payload, **never** LLM-generated.

- **One campaign per process.** No multi-tenant logic, no cross-campaign state, no connection pool to a shared DB service. Multi-campaign hosts spawn one process per campaign, one SQLite file per campaign.

- **No alignment axis on species or archetypes.** No "evil orcs." Hostility is situational (role + faction + triggers), not species-intrinsic. Every hostile encounter's content entry carries at least one peaceful `resolution_path` unless all participants are truly mindless. Enforced at content-review time.

## Per-phase workflow

Every phase in the Roadmap follows this loop:

1. Branch off `main` → `feature/phase-N-<name>`.
2. Implement the phase's tools and supporting code.
3. Write integration tests in `tests/` covering the E2E assertions listed in the Roadmap table for that phase. Use `cargo test` with subprocess-spawned binary + real MCP client (`rmcp` client-side for stdio; `reqwest` for HTTP). No Playwright — there is no browser surface.
4. `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` — all pass clean.
5. Update README.md — tick the phase's checkbox in the Roadmap table, link the PR.
6. Push branch → `gh pr create --base main` with a description summarising scope and assertions.

Do not merge multiple phases in one PR. Each phase is a discrete, reviewable slice.

## Key architectural decisions (pointers)

Full rationale is in [`docs/`](docs/README.md); brief pointers here so a fresh session doesn't relitigate closed questions:

- **Dual transport from day one.** stdio + HTTP via a transport-agnostic `dispatch.rs` core. MCP SDK is `rmcp` (official). See [`docs/architecture.md`](docs/architecture.md).
- **Single polymorphic `events` table** with JSON payload + `event_participants`/`event_items` junctions. Append-only. Code-templated summaries. See [`docs/history-log.md`](docs/history-log.md).
- **Two-tier time** — `campaign_hour` (cumulative world-hours, integer, may be negative for pre-campaign backstory) + nullable `combat_round` (6s ticks inside combat). The clocks do not reconcile arithmetically. See [`docs/history-log.md`](docs/history-log.md).
- **Unified `characters` table** for player, companions, pets, friendly NPCs, enemies. Role is a column, not a table split. Pets use the full schema. See [`docs/characters.md`](docs/characters.md).
- **Effects never mutate base stats** — they are rows with `modifier` and `dice_expr` that compose at read time. Conditions are separate; their mechanical riders live in `content/rules/conditions.yaml`. See [`docs/checks.md`](docs/checks.md).
- **Ideology alignment is a universal social-check modifier**, not companion-specific. Tiers fixed in `content/rules/ideology_alignment.yaml`. See [`docs/checks.md`](docs/checks.md).
- **Graph-of-zones world**, not hex grid. `direction_from` on connections is forward-compat. Lazy generation (stub on adjacency, full on first visit) with a reconciliation pass that prefers existing entities when filling slots. See [`docs/world.md`](docs/world.md).
- **Goal-not-kills XP.** `encounter.goal_completed` fires XP regardless of path; bypassing an encounter earns the same as fighting it. See [`docs/encounters.md`](docs/encounters.md).
- **Combat is a mode of an encounter**, not a separate subsystem. `combat.start` auto-ends any other in-combat encounter to prevent stale state. See [`docs/encounters.md`](docs/encounters.md).

## Current stub caveats (removed by Phase 1)

Until Phase 1 merges, these are artifacts of the pre-redesign code:

- `Cargo.toml` declares `rust-mcp-sdk`, `rust-mcp-transport`, `hyper`, `rustls`, `tokio-rustls` — all **unused in `src/`**. Phase 1 replaces with `rmcp` and trims unused deps.
- `src/httpstream.rs` is a stub that prints a placeholder message instead of serving HTTP.
- `src/dice_test.rs` is dead code — not declared as a module, references an older API that no longer compiles. Live tests are in the `#[cfg(test)] mod tests` block inside `src/dice.rs`.
- The existing stdio loop is bespoke newline-delimited JSON, **not** real MCP protocol.

Do not rely on anything the current code seems to imply — treat `main` as "has a working dice roller and that's it."

## Environment / config

Configuration is entirely via `DMMCP_`-prefixed environment variables loaded once at startup into a `Config` struct. The full table lives in [README.md](README.md#configuration). `foreign_keys=ON` is hard-coded for correctness; everything else is tunable.

Default DB path is `./campaign.db`. One campaign per process. Override with `DMMCP_DB_PATH`.

## Build & run (commands)

Standard Cargo — nothing fancy:

```
cargo build --release
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

Runtime:

```
dm-mcp stdio   # MCP over stdin/stdout
dm-mcp http    # MCP over HTTP (serves /healthz)
```
