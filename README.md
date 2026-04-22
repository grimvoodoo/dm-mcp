# dm-mcp

An MCP (Model Context Protocol) toolkit for AI Dungeon Masters running solo RPG campaigns. One human player, an LLM DM, and this server holding the world's persistent state.

## Status

**Pre-MVP.** Design complete — full rationale in [`docs/`](docs/README.md). Implementation proceeds through the phased [Roadmap](#roadmap) below. The `main` branch still carries the original dice-rolling stub; phase branches land via PR. Features described elsewhere in this README are the designed product; consult the Roadmap for what ships when.

## Roadmap

Implementation is broken into 10 small, testable phases. Each phase ships a vertical slice with E2E integration tests (`cargo test`, subprocess-spawned binary driven through real MCP protocol — see [docs/architecture.md](docs/architecture.md)).

### Per-phase workflow

1. Branch off `main` → `feature/phase-N-<name>`.
2. Implement the phase's tools and supporting code.
3. Write integration tests in `tests/` covering the E2E assertion(s) in the table below. Unit-test any non-trivial internal function.
4. `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` — all clean.
5. Tick the phase's checkbox in the table below and link the PR.
6. Push branch → `gh pr create --base main` with a description summarising scope and assertions.

Do not merge multiple phases in one PR; each phase is a discrete reviewable slice.

### Phases

| # | Phase | Scope | E2E assertion | Status | PR |
|---|-------|-------|---------------|--------|----|
| 1 | **Skeleton** | Binary builds; `dm-mcp stdio` and `dm-mcp http` subcommands; `Config` loaded from `DMMCP_` env vars; HTTP serves `/healthz`; MCP handshake over stdio; trivial `server.info` tool; old stub code, `httpstream.rs`, `dice_test.rs`, and unused deps (`rust-mcp-sdk`, `rust-mcp-transport`, `hyper`, `rustls`, `tokio-rustls`) removed; `rmcp` added. | Spawn HTTP → `GET /healthz` = 200. Spawn stdio → MCP handshake succeeds → `server.info` returns expected shape (version + transport). | ☑ | [#3](https://github.com/grimvoodoo/dm-mcp/pull/3) |
| 2 | **DB + content loader** | Schema migrations for all tables from the design; SQLite connection opened with configured PRAGMAs; bundled YAML content for `abilities`, `skills`, `damage_types`, `conditions`, plus one sample biome, weapon base, enchantment, and archetype; content parsed once at startup into typed structs; `content.introspect` tool. | Delete DB file → start server → file exists with all expected tables. `content.introspect` response contains expected content IDs. | ☑ | [#5](https://github.com/grimvoodoo/dm-mcp/pull/5) |
| 3 | **Dice** | `dice.roll` tool: standard dice (d4–d100), arbitrary ranges (`11-43`), multi-dice (`3d6`) with per-die results and sum. | `dice.roll("d20")` → result ∈ [1, 20]. `dice.roll("3d6")` → 3 rolls, sum equals total. `dice.roll("11-43")` → result ∈ [11, 43]. | ☑ | [#6](https://github.com/grimvoodoo/dm-mcp/pull/6) |
| 4 | **Character core** | `character.create`, `character.get` (effective stats with active effects composed), `character.update_plans`, `character.change_role`; `character_proficiencies` CRUD; `effects` table: `apply_effect`, `dispel_effect`; `character_resources` CRUD. | Create character with STR 14 → `apply_effect(+4 STR)` → `character.get` shows effective 18 → `dispel_effect` → shows 14. Event log has `character.created`, `effect.applied`, `effect.expired`. | ☑ | [#7](https://github.com/grimvoodoo/dm-mcp/pull/7) |
| 5 | **Checks** | `resolve_check` composing base + ability mod + proficiency + ranks + effects + conditions + caller modifiers; condition mechanical riders loaded from `content/rules/conditions.yaml`; ideology-alignment modifier threaded via `check_spec.modifiers`. | Apply Bless (`1d4` on checks) → resolve persuasion → breakdown contains rolled bless die. Apply blinded → resolve attack → two d20s rolled, lower taken. Pass `{kind: ideology_alignment, value: -6}` → event payload records the modifier with reason. | ☑ | [#8](https://github.com/grimvoodoo/dm-mcp/pull/8) |
| 6 | **Campaign setup + starting zone** | `setup.new_campaign`, `setup.answer`, `setup.generate_world` (starting zone + 2–5 stub neighbours, no NPCs yet), `setup.mark_ready`; `campaign_state` singleton transitions setup → running, `campaign_hour` = 0. | Full setup flow (new → 3 answers → generate → ready) → DB has starting zone matching biome answer, 2–5 stub neighbour zone rows, `campaign_state.phase='running'`, `campaign.started` event at hour 0. | ☐ | – |
| 7 | **Travel + fog of war** | `world.travel`, `world.map`, `world.describe_zone`; entering a zone triggers stub-generation for missing neighbours; first-visit full generation creates landmarks (NPC placement deferred to Phase 8). | Travel to neighbour → `campaign_hour` advanced by edge's `travel_time_hours`, `character_zone_knowledge.level` = 'visited', `world.map` returns both zones with computed 2D positions and connection. | ☐ | – |
| 8 | **NPC generation + recall** | `npc.generate` with two committed archetypes (one friendly, one enemy); backstory synthesis with 3–5 events at negative `campaign_hour`; reconciliation pass fills slots from existing characters/zones; `character.recall` with filters; zone full-generation now places NPCs. | Generate orc_raider in a zone → character row has stats in archetype range, proficiencies inserted, loadout items held. Event log has 3–5 `history.backstory` events with `campaign_hour < 0` and the orc as a participant. `character.recall(orc.id)` returns those events. | ☐ | – |
| 9 | **Encounters + combat + death** | `encounter.create`, `encounter.complete` (XP flow by resolution path), `encounter.abandon`, `encounter.fail`; `combat.start` with stale-combat auto-cleanup, `combat.next_turn` with round-based effect/condition expiry, `combat.end`; `combat.apply_damage` integrates the death flow (`mortally_wounded` at 0 HP → `roll_death_save` → `roll_death_event` on 3 failures); short/long rest tools. | Start encounter → combat → `next_turn` × N with 2-round effect → expired after round 3. Damage to 0 HP → `mortally_wounded` applied, status='unconscious'. Three failed death saves → status='dead' → `roll_death_event` returns a rolled event. Starting a second combat while first still flagged → first auto-ended, `combat.auto_ended` emitted. | ☐ | – |
| 10 | **Inventory + barter + encumbrance** | Full items tool surface: `inventory.create/transfer/pickup/drop/equip/unequip/get/inspect`; weight computation; encumbrance enforcement (`encumbered` condition between 67% and 100%, pickup refused above 100%); `barter.exchange` with persuasion-check-driven rate. | STR 10 (capacity 150 lb) → pickup 100 lb → no condition. Pickup 10 more → 73% of capacity → `encumbered` applied. Pickup 50 more (would be 160) → refused with `would_overload`. Barter: offer below fair value → persuasion check → success completes, failure → merchant declines. | ☐ | – |


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
