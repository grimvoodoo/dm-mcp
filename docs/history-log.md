# History & event log

The event log is the **connective tissue** of the world. It powers:

- **NPC backstory generation** — sampling prior events to give newly-generated NPCs 3–5 cross-references to other entities.
- **Recognition scenes** — "the villagers recognise this orc because it raided them" is a join over the log.
- **Player recall** — "what did I do in the tower last session" is a character-scoped log query.
- **XP arithmetic** — character XP is the sum of `xp.*` events for that character.
- **Effect / condition lifecycles** — every effect links back to the event that applied it; every expiry is an event.
- **Audit trail** — append-only, so the DM agent can always trust what it reads.

## Schema

One polymorphic event table plus two junction tables for indexed "events involving X" queries:

```
events(
    id              PK,
    kind            TEXT,       -- dotted taxonomy: combat.hit, xp.goal, effect.applied, ...
    campaign_hour   INTEGER,    -- cumulative in-game hours since campaign start (may be negative)
    combat_round    INTEGER?,   -- set only for events inside combat
    zone_id         FK?,        -- null for global / cross-zone events
    encounter_id    FK?,        -- null outside encounters
    parent_id       FK?,        -- ties sub-events to their parent (a resolved check → individual rolls)
    summary         TEXT,       -- one-line prose the LLM reads directly
    payload         TEXT        -- JSON; schema varies by kind
)

event_participants(
    event_id        FK,
    character_id    FK,
    role            TEXT,       -- actor | target | witness | beneficiary
    PRIMARY KEY (event_id, character_id, role)
)

event_items(
    event_id        FK,
    item_id         FK,
    role            TEXT,       -- same roles as above; items can be 'stolen', 'given', 'destroyed', etc.
    PRIMARY KEY (event_id, item_id, role)
)
```

## Design decisions

### Polymorphic `events` table, not per-kind satellite tables

A single `events` table with a JSON `payload` column is much easier to extend than a family of satellite tables. New event kinds only need a new string for `kind`; no migrations.

The cost of JSON payload is that type-specific fields aren't trivially queryable. That's fine for this project — the **cross-entity queries** (recognition, backstory, recall) only need entity filters, and those go through `event_participants`/`event_items` junction tables which are indexed.

### `kind` is a dotted taxonomy, not an enum

`combat.hit`, `combat.miss`, `combat.death`, `xp.goal`, `xp.bonus`, `effect.applied`, `effect.expired`, `stat.change`, `inventory.transfer`, `location.move`, `social.persuade`, `social.bargain`, `social.refusal`, `discovery.secret`, `discovery.landmark`, `encounter.start`, `encounter.goal_completed`, `encounter.abandoned`, `combat.start`, `combat.next_turn`, `combat.end`, `combat.auto_ended`, `history.backstory`, `history.relationship.family`, `npc.role_changed`, `npc.plan_changed`, `meta.retraction`, …

Filtering is broad (`kind LIKE 'combat.%'`) or exact (`kind = 'combat.hit'`). Adding a new kind is a string; no migration, no enum edit.

### `summary` is code-templated, not LLM-generated

The `summary` column is mandatory prose such as *"Kira hit the orc for 7 damage with a rusted axe."* It is produced in Rust from `kind` + `payload` at event insert time.

**Do not use an LLM call to generate summaries.** An LLM call per event would destroy the latency budget, and summaries are formulaic enough that a template renderer produces equivalent-quality text.

### `parent_id` gives event hierarchies

A `check.resolve` event is the parent of the individual dice-roll events that produced it (if those are emitted as separate events at all — see below). An encounter's `encounter.start` … `encounter.goal_completed` pair wraps many child events.

This lets the agent request "the whole check, with rolls" or "everything that happened in encounter 91" as a tree query, and lets coarse summaries cite fine-grained detail on demand.

### Rolls live in the parent check's payload, not as child events

Default: a `check.resolve` event with `payload.rolls = [{d20: 14, mods: [{...}]}]` captures everything needed for audit without inflating the event table. Child events are reserved for sub-events with their own narrative weight (a critical hit's extra damage roll, a triggered saving throw).

This keeps event volume manageable without losing fidelity.

### Append-only; retractions are new events

Never update or delete event rows. A correction is a new event with `kind='meta.retraction'` and `parent_id=<bad>`. This makes the log trustworthy — the agent can always believe what it reads — and keeps stat/effect reconstructions deterministic.

### Indexes

```
event_participants(character_id, event_id)     # "events involving X" — the hottest path
events(zone_id, campaign_hour DESC)            # "recent events in this zone"
events(encounter_id)                           # assembling an encounter's full history
events(parent_id)                              # walking event trees
events(kind, campaign_hour DESC)               # broad kind-filtered queries
```

## Two-tier time model

```
events.campaign_hour    INTEGER NOT NULL    -- cumulative in-game hours
events.combat_round     INTEGER NULL        -- set only for events inside combat
```

### World time

`campaign_hour` is an integer: cumulative hours since campaign start.

- **Campaign start is `campaign_hour = 0`**, the moment `setup.mark_ready` fires.
- **Negative values = pre-campaign fictional history** — used by NPC backstory synthesis to place events "in the past, before the campaign began." Easy to filter out of "recent events" queries.
- Day/night and calendar are derivable: `campaign_hour % 24` gives hour-of-day, integer division by 24 gives day number, and so on. No separate calendar table.
- Hour is the minimum world-time granularity. Resting advances the clock in 8-hour jumps; travel in multi-hour segments; encounters in authored-duration blocks.

### Combat time

`combat_round` is nested inside combat. A round is six in-game seconds. While `encounters.in_combat = true`, every event emitted carries `combat_round = encounters.current_round`; events outside combat have `combat_round = NULL`.

**The two clocks do not reconcile arithmetically.** A 20-round brawl does not cost 120 real-world seconds of world time — the encounter's world-time cost is whatever it was authored for. Combat rounds exist solely for in-combat effect/condition expiry bookkeeping and initiative tracking.

### Encounter duration

Encounters carry an `estimated_duration_hours` field set at generation. When an encounter closes (`encounter.goal_completed`, `encounter.abandoned`, etc.), the world clock advances by that (or a DM-adjusted actual delta, logged on the closing event). Example: a 6-hour tower entered at dawn (`campaign_hour=1248`) closes at `campaign_hour=1254` with plenty of daylight left for another event.

## Example event chain

A bribe that fails because the guard has strong ideology:

```
events:
  id=847, kind='social.bargain', campaign_hour=1203, encounter_id=91, parent=NULL
          summary='Kira offered the guard 10g to let her pass'
          payload={"offer":10, "currency":"gold"}
  id=848, kind='check.resolve', parent=847
          summary='Kira persuasion check failed (14 vs DC 18)'
          payload={"skill":"persuasion","dc":18,"total":14,"rolls":[{"d20":8}],
                   "modifiers":[{"kind":"ideology_alignment","value":-2,
                                 "reason":"guard's duty vs request to shirk it"}]}
  id=849, kind='social.refusal', parent=847
          summary='The guard refused and called for backup'

event_participants:
  (847, Kira, actor)    (847, guard, target)
  (848, Kira, actor)
  (849, guard, actor)   (849, Kira, target)
```

The full breakdown — DC, rolls, modifier rationale — is captured in the payload. The three chained summaries make the story readable without the agent ever touching JSON.

## Scope

**Single table, indexed.** Per-fight partitioning was considered and rejected: the cross-entity queries that matter (recognition, backstory sampling, recall) would become N+1 `UNION ALL` joins across per-fight tables, which fight the query planner and are slower than one indexed table.

**If event volume ever becomes a real problem**, split by event *hotness*, not by encounter boundary: keep the lean `events` header table hot and offload wide combat-roll detail to a secondary `event_detail_rolls` table. Don't add this pre-emptively.
