# Architecture

## Consumer

`dm-mcp` is an MCP (Model Context Protocol) toolkit whose **only consumer is an LLM DM agent**. There is no human-DM prose-rendering mode. Every tool returns structured JSON; the DM agent writes narration.

The server sits in the hot path of every player turn:

```
player → DM agent → dm-mcp tools → DM agent → player
```

Latency compounds with LLM latency, which is already high. Every design decision in this doc weighs latency as a first-class goal.

## Deployment targets

- **Raspberry Pi** (home-hosted personal play)
- **Server** (beefier self-host)
- **Kubernetes pod** (homelab deployment from day one — the user runs K8s at home)

The server is shipped as both a **statically-linked binary** and a **`scratch` container image** (~10–20 MB). No libc, no libssl, no external services. This means:

- **No OpenSSL.** All TLS goes through `rustls`. OpenSSL breaks scratch container builds. If a new dependency silently pulls `native-tls` or `openssl-sys`, that is a build-blocker — swap the dep or select a rustls feature.
- **One campaign per process.** Multi-campaign hosts spawn one process per campaign. Keeps the connection pool, prepared-statement cache, and content cache simple; avoids cross-campaign state hazards.
- **SQLite** rather than a sidecar database service. One `campaign.db` file per campaign, bind-mounted or PVC'd in Kubernetes.

## Transport layer

Both **stdio** and **HTTP** are supported from day one. Local DM-agent setups use stdio for lowest latency; Kubernetes deployments use HTTP.

Architecture is a **transport-agnostic dispatch core** with thin transport shells on top:

```
src/
  core/
    dispatch.rs       # fn dispatch(tool, args) -> ToolOutput
    tools/            # one module per tool; call into domain modules
  transport/
    stdio.rs          # MCP framed JSON over stdin/stdout
    http.rs           # streamable HTTP (or SSE) server + /healthz
```

Each transport parses the MCP envelope, calls `dispatch`, writes the response back. No business logic in transport.

### Health endpoint

HTTP mode exposes `GET /healthz` which returns 200 when the SQLite database is reachable. Wire it as both a readiness and a liveness probe in Kubernetes.

### MCP SDK

**`rmcp`** (the official Rust SDK from the Model Context Protocol organisation). Chosen because the official SDK tracks protocol changes fastest. The legacy `rust-mcp-sdk` and `rust-mcp-transport` dependencies in the pre-redesign `Cargo.toml` are to be removed.

### CLI shape

```
dm-mcp stdio     # MCP over stdin/stdout
dm-mcp http      # MCP over streamable HTTP
```

## Latency commitments

Non-negotiable defaults chosen for the hot path:

- **SQLite PRAGMAs set at connection open.** `journal_mode=WAL`, `synchronous=NORMAL`, `mmap_size` (64 MB Pi / 256 MB+ server), `cache_size` (32–128 MB), `foreign_keys=ON` (hard-coded; correctness, not tuning).
- **Prepared statement caching** (`rusqlite::Connection::prepare_cached` or `sqlx`'s equivalent) for every hot query. Parse/plan work runs once.
- **Batched writes per tool call.** A single call often emits several related events (`encounter.start` + `check.resolve` + `xp.bonus`); wrap them in one transaction. One fsync instead of many — large Pi-SD-card win.
- **Content tables parsed once at startup** into in-memory structs. Bundled via `include_str!`; tool-call hot path touches no disk for content lookups.
- **Code-templated event summaries, not LLM-generated.** Every event's `summary` field is produced in Rust from `kind` + `payload`. An LLM call per event write would destroy the budget.
- **Trim the dependency tree.** Binary size and cold-start matter in containers. Every new crate gets audited for necessity and TLS backend.

## Environment-variable configuration

Every performance knob has a low-latency default and is overridable via `DMMCP_`-prefixed environment variables. A single `Config` struct is loaded once at startup and threaded to the connection opener and the HTTP server.

| Variable                  | Default         | Effect                                                                 |
|---------------------------|-----------------|------------------------------------------------------------------------|
| `DMMCP_DB_PATH`           | `./campaign.db` | Path to the SQLite file for this campaign                              |
| `DMMCP_DB_JOURNAL_MODE`   | `WAL`           | SQLite `journal_mode` PRAGMA                                           |
| `DMMCP_DB_SYNCHRONOUS`    | `NORMAL`        | SQLite `synchronous` PRAGMA                                            |
| `DMMCP_DB_MMAP_SIZE`      | `67108864`      | SQLite `mmap_size` PRAGMA, bytes (64 MB)                               |
| `DMMCP_DB_CACHE_SIZE`     | `-32768`        | SQLite `cache_size` PRAGMA. Negative = kilobytes (32 MB)               |
| `DMMCP_HTTP_BIND`         | `0.0.0.0`       | HTTP transport bind address                                            |
| `DMMCP_HTTP_PORT`         | `3000`          | HTTP transport port                                                    |
| `DMMCP_LOG_LEVEL`         | `info`          | `tracing` log level (`trace`/`debug`/`info`/`warn`/`error`)            |
| `DMMCP_CONTENT_DIR`       | *(unset)*       | Overrides bundled content with on-disk content directory (dev/custom)  |

`foreign_keys=ON` is not exposed as a knob — correctness, not tuning.

## What's not deployed

- Network calls to external services (the MCP is self-contained).
- Background workers, schedulers, cron-like workloads (everything is request-response; autonomous NPC behaviour between sessions is explicitly out of scope for MVP).
- Authentication and multi-tenancy (one campaign per process; security boundary is the container/process boundary).
