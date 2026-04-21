# dm-mcp

An MCP (Model Context Protocol) toolkit for AI Dungeon Masters running solo RPG campaigns. One human player, an LLM DM, and this server holding the world's persistent state.

## Status

**Pre-MVP.** Active redesign on branch `feature/re-design`. The main branch currently exposes only a dice-rolling stub over a bespoke stdio JSON loop; nothing below describes the current code. Features listed are the designed product, not the shipped one.

## What it does (designed)

- **Dice** — d4, d6, d8, d10, d12, d20, d100, arbitrary ranges (`11-43`), multi-dice (`3d6`), modifiers, advantage/disadvantage
- **World generation** — tag-based biome and encounter tables, hex-grid maps with fog of war, landmarks
- **Characters** — one unified model for the player, companions (adventurers and pets), and NPCs; skills as rows for flexible progression; loyalty + ideology for obedience mechanics
- **Items** — base types layered with materials (+1 basic → +5 exotic) and composable enchantments
- **Checks** — skill-check resolution with modifier stacking (including active temporary effects)
- **History / event log** — append-only record of everything that happens; seeds NPC backstories, powers recognition scenes ("the villagers recognise this orc"), and backs XP arithmetic
- **XP** — awarded on encounter-goal completion (no "kill XP" — stealth past the skeletons earns the same as slaying them) plus DM-discretionary bonuses for clever play

The consumer is always an LLM DM agent. Tool responses are structured JSON; narration is the agent's job.

## Build

```bash
cargo build --release
```

Pure Rust, rustls-only TLS — the release binary is statically linked and ships into a `scratch` container with no system dependencies.

## Run

```bash
dm-mcp stdio   # MCP over stdin/stdout (fastest; for local DM agents)
dm-mcp http    # MCP over streamable HTTP (for Kubernetes / networked deploys)
```

One campaign per process. Multi-campaign hosts spawn one process per campaign.

## Configuration

Every performance knob has a low-latency default and is overridable via environment variables. Defaults are tuned for the hot path — the MCP sits between every player command and the DM agent's response, so latency compounds with LLM latency.

| Variable                  | Default         | Effect                                                                 |
|---------------------------|-----------------|------------------------------------------------------------------------|
| `DMMCP_DB_PATH`           | `./campaign.db` | Path to the SQLite file for this campaign                              |
| `DMMCP_DB_JOURNAL_MODE`   | `WAL`           | SQLite `journal_mode` PRAGMA. WAL lets reads and writes run concurrently |
| `DMMCP_DB_SYNCHRONOUS`    | `NORMAL`        | SQLite `synchronous` PRAGMA. `NORMAL` is durable across crashes and faster than `FULL` under WAL |
| `DMMCP_DB_MMAP_SIZE`      | `67108864`      | SQLite `mmap_size` PRAGMA, bytes. Default 64 MB; raise to 256 MB+ on larger hosts |
| `DMMCP_DB_CACHE_SIZE`     | `-32768`        | SQLite `cache_size` PRAGMA. Negative values are kilobytes; default is 32 MB |
| `DMMCP_HTTP_BIND`         | `0.0.0.0`       | HTTP transport bind address                                            |
| `DMMCP_HTTP_PORT`         | `3000`          | HTTP transport port                                                    |
| `DMMCP_LOG_LEVEL`         | `info`          | `tracing` log level (`trace`, `debug`, `info`, `warn`, `error`)        |

`foreign_keys = ON` is hard-coded — that's correctness, not tuning.

Tinker at your own risk. The defaults are good for a Raspberry Pi 4 running one campaign; a beefier host can turn `DMMCP_DB_MMAP_SIZE` and `DMMCP_DB_CACHE_SIZE` up for better cache behaviour on large campaigns.

## Kubernetes

The `http` mode exposes `GET /healthz` which returns 200 when the SQLite database is reachable — wire it as both a readiness and liveness probe. Mount the campaign database as a PersistentVolume.

A minimal deployment looks like:

```yaml
containers:
  - name: dm-mcp
    image: dm-mcp:latest      # scratch-based, ~10–20 MB
    args: ["http"]
    env:
      - name: DMMCP_DB_PATH
        value: /data/campaign.db
    ports:
      - containerPort: 3000
    readinessProbe:
      httpGet: { path: /healthz, port: 3000 }
    livenessProbe:
      httpGet: { path: /healthz, port: 3000 }
    volumeMounts:
      - name: campaign
        mountPath: /data
volumes:
  - name: campaign
    persistentVolumeClaim:
      claimName: dm-mcp-campaign
```

## License

MIT — see [LICENSE](LICENSE).
