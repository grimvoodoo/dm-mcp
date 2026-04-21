# Characters, parties & death

## One table for every living (or dead) thing

The **same `characters` table** backs the player, every companion, every pet, every friendly NPC, every enemy, every bystander. A `role` field and a few nullable columns are the only distinctions between roles. This is the single most load-bearing decision in the data model.

### Why one table

- **Companions come and go.** A mercenary is hired (friendly), then killed (dead). A rescued villager joins the party (companion), later leaves (friendly). An enemy is defeated, then resurrected as a cursed servant. Moving rows between tables when roles change is painful, and every downstream feature (inventory, checks, history, initiative) wants the same shape for all of them.
- **Pets deserve full statlines.** Players get attached to animal companions and spend potions, spells, and story time growing them into real contributors. A stripped-down pet schema makes "my dog drinks a potion of intellect and is now a full party member" painful to model. Better to give the pet the full schema with low starting stats and empty skill rows that fill in over time.
- **Recognition and history work identically across roles.** "What does this villager know about this orc?" and "what has my companion witnessed of this zone?" are the same query.

### Role taxonomy

```
role ∈ { player | companion | friendly | enemy | neutral }
```

There is exactly one `player` per campaign (solo game). `companion` signals "in the player's party and operates under `issue-an-order` semantics." `friendly`/`enemy`/`neutral` are the social dispositions for NPCs not in the party.

**Roles change via `character.change_role(character_id, new_role, reason)`**, which emits `npc.role_changed` with before/after values and a narrative reason. Side-swaps (enemy becomes ally after learning the truth) are first-class narrative pivots with audit trails.

**No alignment axis.** There is no `good/evil` or `lawful/chaotic` column. Hostility is always situational, not species-essential. See [NPC generation](npcs.md) for how this shapes archetype authoring.

## Schema

```
characters(
    id                    PK,
    name                  TEXT,
    role                  TEXT,            -- player | companion | friendly | enemy | neutral
    party_id              FK?,

    -- Ability scores (BASE values; effective = base + active effect modifiers)
    str_score, dex_score, con_score,
    int_score, wis_score, cha_score     INTEGER,

    -- Combat numbers
    hp_current, hp_max, hp_temp          INTEGER,
    armor_class                          INTEGER,
    speed_ft                             INTEGER,
    initiative_bonus                     INTEGER,

    -- Progression (denormalised for read latency)
    level                                INTEGER,
    xp_total                             INTEGER,
    proficiency_bonus                    INTEGER,

    -- Physical
    size                                 TEXT,          -- tiny..gargantuan

    -- Narrative labels (LLM-interpreted, not mechanised)
    species                              TEXT,
    class_or_archetype                   TEXT,
    ideology                             TEXT,          -- prose for alignment-of-request checks
    backstory                            TEXT,          -- grows over time
    plans                                TEXT,          -- current agenda / motivations

    -- Party mechanics
    loyalty                              INTEGER,       -- 0-100

    -- Lifecycle
    status                               TEXT,          -- alive | unconscious | dead | missing
    current_zone_id                      FK?,
    death_save_successes                 INTEGER,       -- 0-3 while dying
    death_save_failures                  INTEGER,       -- 0-3 while dying

    created_at, updated_at               INTEGER
)

parties(
    id                                   PK,
    name                                 TEXT?,
    created_at                           INTEGER
)
```

Inventory lives in the [items table](items.md). Skills and proficiencies live in [`character_proficiencies`](checks.md). Temporary effects live in [`effects`](checks.md). Conditions live in [`character_conditions`](checks.md). Limited-use resources (spell slots, etc.) live in [`character_resources`](#resources).

## Key decisions

### Ability scores are base values, never mutated

`str_score` through `cha_score` are the **base** numbers. A potion of giant strength does not rewrite `str_score`; it inserts a row in the `effects` table with `target_key='str_score', modifier=+8`. When the effect expires, the row is marked inactive and the base is untouched.

`resolve_check` reads the **effective** stat: base + sum of active modifiers targeting that key. This answers "what were my stats before the curse" for free, and lets stacking effects compose cleanly.

### `xp_total`, `proficiency_bonus`, `level` are denormalised

All three are theoretically derivable (XP from the event log, proficiency bonus and level from a lookup on XP). They are stored on the character row anyway, and updated **atomically in the same transaction** as the triggering XP event insert.

Reading a character's level on every `resolve_check` should not require aggregating the event log. The denormalisation is a latency decision.

### `class_or_archetype` is a free-text label, not an enum

The MCP does not know the difference between a wizard and a sorcerer. It needs ability scores, proficiencies, HP, and whatever resources the character tracks. Class is a label the DM agent interprets — and keeping it free-text avoids enumerating every variant, homebrew class, and edge case.

### `ideology` is prose, not structured

`ideology` is a single string, typically a sentence, that the DM agent reads to decide whether a given request aligns with or opposes the character's values. See the [ideology-alignment rubric](checks.md#ideology-alignment) for how the agent converts this prose into a check DC modifier.

Structuring ideology (alignment enums, tag lists, value sliders) loses nuance and forces the generator to make reductive choices. Prose is cheap to store and the LLM interprets it well.

### `plans` is prose, updated narratively

`plans` describes the character's current agenda — what they're trying to accomplish. Updated via `character.update_plans(character_id, new_plans, source_event_id)`, which emits `npc.plan_changed`. The agent rewrites plans as narrative develops.

## Parties

Parties are a **grouping only** — no shared stats, no shared inventory, no party-level HP. Characters reference their party via `characters.party_id` and each carries their own inventory, history, and location.

`party_id` is **explicit**, not emergent from co-location. Supports split-the-party scenarios: a captured companion's `current_zone_id` differs from the rest of the party; they remain members until explicitly removed.

Shared gold, shared loot, shared decisions — all narrative conventions the agent handles by choosing which party member's inventory to touch.

## Companions

### Issuing orders is a check, not a control primitive

The player does not directly puppet their companions. Ordering a companion to do something is modelled as a skill check **on the companion**, using:

- **Loyalty** (the numeric `characters.loyalty` column — persistent relationship with the player)
- **Ideology alignment** (the [rubric-driven modifier](checks.md#ideology-alignment) for this specific request)
- **Situational modifiers** (distance, stress, conflicting orders)

Telepathy, rangefinding spells, or "talk to animals" are spells that waive proximity or language requirements — they're content data, not a separate subsystem.

### Pets use the full character schema

No `agency` tier, no reduced statline. A dog gets the same columns as a paladin: `str..cha`, HP, AC, skills, resources. A new pet has `int=3, ranks in bite=1`; a heavily-invested pet has grown stats and a wide skill list. The data model doesn't care.

### Loyalty vs ideology

- **Loyalty** is numeric (0–100) and persistent. It captures the character's relationship with the player — earned through shared events (rescued, gifted, fought alongside), lost through betrayal or mistreatment.
- **Ideology** is prose and per-character. It captures the character's values — stable across time, informs how they interpret specific requests.

The two are **orthogonal**. A fiercely loyal companion can refuse a deeply misaligned order (`loyalty: +4, alignment: -10` → net -6, likely failure). A stranger with favourable alignment can cooperate on first meeting (`loyalty: 0, alignment: +3` → +3, easy success).

## Resources

Limited-use per-rest resources live in a generic table:

```
character_resources(
    character_id    FK,
    name            TEXT,        -- 'slot:1', 'slot:9', 'hit_die', 'mana', 'ki', 'rage_use'
    current         INTEGER,
    max             INTEGER,
    recharge        TEXT,        -- short_rest | long_rest | dawn | never | manual
    PRIMARY KEY (character_id, name)
)
```

Spell slots (`slot:1` through `slot:9`), sorcery points, ki points, rage uses, bardic inspiration, superiority dice, hit dice for short rests — all live here. The agent knows what each namespace means.

`recharge` values are processed by rest tools (`character.short_rest`, `character.long_rest`) which iterate the resource rows and refill matching ones.

## Progression

Experience is awarded via events (see [history log](history-log.md)), never by kills:

- `encounter.goal_completed(encounter_id, character_id, xp)` — fires on goal completion regardless of path. Stealth past the skeletons earns the same XP as fighting them.
- `xp.bonus(character_id, xp, reason)` — DM-discretionary awards for clever play.

Character XP = sum of these events for the character. The `xp_total` column is kept in sync atomically with the event inserts.

Level is derived from XP via a content-driven lookup table; `proficiency_bonus` is derived from level. Both are denormalised on the character row, and the `character.level_up(character_id)` tool updates all three atomically.

## Death

Three-stage state machine, enforced by the MCP:

### Stage 1 — HP drops to 0

- `apply_condition(mortally_wounded, character_id, …)`
- `status = 'unconscious'`
- `hp_current = 0` (never negative)
- Event emitted so the DM agent knows the character is down

### Stage 2 — Death saves

When the DM calls `roll_death_save(character_id)`:

- d20 rolled
- `≥ 10` → success (increment `death_save_successes`)
- `< 10` → failure (increment `death_save_failures`)
- **Natural 20** → auto-stabilise; `status='alive'`, `hp_current=1`, counters reset
- **Natural 1** → counts as two failures

Three successes → stabilised (`status='alive'`, `hp_current=1`, counters reset).
Three failures → `status='dead'`; trigger stage 3.

### Stage 3 — Full death

Because this is a solo RPG, "dead" ends the adventure unless something intervenes. The DM calls `roll_death_event(character_id)`, which rolls on a weighted table in `content/rules/death_events.yaml`:

```yaml
- weight: 2
  kind: audience_with_death_god
  description: "The character finds themselves in the hall of the god of the dead..."
  outcome_hooks: [bargain, quest, servitude]
- weight: 5
  kind: plain_end
  description: "The character's story ends."
- weight: 1
  kind: resurrected_by_ally
  description: "A companion's grief or a stranger's kindness..."
  requires: [companion_nearby_or_zone_has_cleric]
- weight: 1
  kind: cursed_return
  description: "Something brings them back, but they're changed."
  outcome_hooks: [permanent_condition]
```

The MCP returns the rolled event and its hooks; the DM agent writes the narrative and calls follow-up tools (`bargain` → new quest; `cursed_return` → `apply_condition` with no expiry; etc.). "Three strikes and you're out" stays cleanly enforced by the MCP; "what death means narratively" stays in content and the agent's hands.

## Tools

| Tool                                            | Effect                                                         |
|-------------------------------------------------|----------------------------------------------------------------|
| `character.create(...)`                         | Usually invoked via `npc.generate` or campaign setup.          |
| `character.get(character_id)`                   | Full readout: stats, effective modifiers, conditions, resources|
| `character.recall(character_id, filters)`       | Events involving this character. See [history](history-log.md). |
| `character.update_plans(character_id, plans, source_event_id)` | Overwrite `plans`, emit `npc.plan_changed`.      |
| `character.change_role(character_id, new_role, reason)` | Role swap with audit event.                             |
| `character.level_up(character_id)`              | Bump level + proficiency_bonus, apply class HP gain.           |
| `character.short_rest(character_id)`            | Refill resources with `recharge='short_rest'`.                 |
| `character.long_rest(character_id)`             | Refill resources with `recharge ∈ {short_rest, long_rest, dawn}`. Heal HP. |
| `roll_death_save(character_id)`                 | Death-save roll; advances the death state machine.             |
| `roll_death_event(character_id)`                | Fires only after three death-save failures.                    |
