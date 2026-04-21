# dm-mcp

An MCP (Model Context Protocol) toolkit for AI Dungeon Masters running solo RPG campaigns. One human player, an LLM DM, and this server holding the world's persistent state.

## Status

**Pre-MVP.** Active redesign on branch `feature/re-design`. The main branch still carries a dice-rolling stub; design work for the full toolkit has landed in [`docs/`](docs/README.md) and implementation is underway. Features listed here describe the designed product, not the shipped one.

## What it does

- **Dice** — d4, d6, d8, d10, d12, d20, d100, arbitrary ranges (`11-43`), multi-dice (`3d6`), modifiers, advantage/disadvantage
- **World generation** — tag-based biome and encounter tables, graph-of-zones with fog of war, landmarks, lazy expansion as the player explores
- **Characters** — one unified model for the player, companions (adventurers and pets), friendly NPCs, and enemies; skills as rows for flexible progression; loyalty + ideology for companion obedience and social-check modifiers
- **Items** — base types layered with materials (tier 1 basic → tier 5 exotic) and composable enchantments; currency is just items; weight-based encumbrance enforced
- **Checks** — d20-inspired check resolution with modifier stacking, active effect composition, and condition riders
- **History / event log** — append-only record of everything that happens; powers NPC backstory generation, recognition scenes, XP arithmetic, and player recall
- **XP** — awarded on encounter-goal completion (no "kill XP" — stealth past the skeletons earns the same as slaying them) plus DM-discretionary bonuses for clever play
- **Non-violent resolution** — every hostile encounter carries at least one peaceful path; no "evil by default" species
- **Campaign bootstrap** — guided setup dialogue at campaign start; world seeded with pre-history events for a lived-in feel from session one

The consumer is always an LLM DM agent. Tool responses are structured JSON; narration is the agent's job.

## Design & documentation

Every architectural decision, schema, and tool is documented in [`docs/`](docs/README.md):

- [Architecture](docs/architecture.md) — transport, deployment, latency, config
- [History & event log](docs/history-log.md)
- [Characters, parties & death](docs/characters.md)
- [Checks, effects & conditions](docs/checks.md)
- [Items & inventory](docs/items.md)
- [World, zones & maps](docs/world.md)
- [Encounters & combat](docs/encounters.md)
- [NPC generation](docs/npcs.md)
- [Campaign setup](docs/campaign-setup.md)
- [Content](docs/content.md)
- [IP & licensing](docs/ip-and-licensing.md)

If you're modifying the project, read the docs for the area you're touching — they include rationale for the design choices, not just the schema.

## Build

```bash
cargo build --release
```

Pure Rust, rustls-only TLS — the release binary is statically linked and ships into a `scratch` container with no system dependencies.

## Run

```bash
dm-mcp stdio    # MCP over stdin/stdout (lowest latency; local DM agents)
dm-mcp http     # MCP over streamable HTTP (networked / Kubernetes deploys)
```

One campaign per process. Multi-campaign hosts spawn one process per campaign, one SQLite file per campaign.

## Configuration

Every performance knob has a low-latency default and is overridable via environment variable. Defaults are tuned for the hot path — the MCP sits between every player command and the DM agent's response, so latency compounds with LLM latency.

| Variable                  | Default         | Effect                                                                                 |
|---------------------------|-----------------|----------------------------------------------------------------------------------------|
| `DMMCP_DB_PATH`           | `./campaign.db` | Path to the SQLite file for this campaign                                              |
| `DMMCP_DB_JOURNAL_MODE`   | `WAL`           | SQLite `journal_mode` PRAGMA. WAL lets readers and writers run concurrently            |
| `DMMCP_DB_SYNCHRONOUS`    | `NORMAL`        | SQLite `synchronous` PRAGMA. Durable across crashes; meaningfully faster than `FULL` under WAL |
| `DMMCP_DB_MMAP_SIZE`      | `67108864`      | SQLite `mmap_size` PRAGMA, bytes. Default 64 MB; raise to 256 MB+ on larger hosts      |
| `DMMCP_DB_CACHE_SIZE`     | `-32768`        | SQLite `cache_size` PRAGMA. Negative values are kilobytes; default is 32 MB            |
| `DMMCP_HTTP_BIND`         | `0.0.0.0`       | HTTP bind address                                                                      |
| `DMMCP_HTTP_PORT`         | `3000`          | HTTP port                                                                              |
| `DMMCP_LOG_LEVEL`         | `info`          | `tracing` log level (`trace`/`debug`/`info`/`warn`/`error`)                            |
| `DMMCP_CONTENT_DIR`       | *(unset)*       | When set, loads bundled YAML content from disk instead of the baked-in copy            |

`foreign_keys = ON` is hard-coded — correctness, not tuning.

Tinker at your own risk. The defaults are good for a Raspberry Pi 4 running one campaign; a beefier host can turn `DMMCP_DB_MMAP_SIZE` and `DMMCP_DB_CACHE_SIZE` up for better cache behaviour on large campaigns. See [docs/architecture.md](docs/architecture.md) for the reasoning behind the latency commitments.

## Kubernetes

`http` mode exposes `GET /healthz` which returns 200 when the SQLite database is reachable. Wire it as both a readiness and a liveness probe. Mount the campaign database as a PersistentVolume.

Minimal deployment:

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
