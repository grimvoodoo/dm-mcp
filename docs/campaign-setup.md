# Campaign setup (bootstrap phase)

Every run of `dm-mcp` begins with a setup phase: a guided dialogue between the DM agent and the player that produces enough information for the world-generator to seed a starting region. Without this phase, every campaign would start in a generic forest with a generic hook. With it, each run feels bespoke.

The setup is a first-class feature, not an afterthought. It's small — a handful of tools and one content file — but it's the hinge on which campaign-to-campaign variety turns.

## Flow

1. **`setup.new_campaign()`** creates an empty database at `DMMCP_DB_PATH`, initialises the schema, and returns the list of setup questions the agent should walk the player through.
2. **`setup.answer(question_id, answer)`** records each player response as it comes in. Answers live in a lightweight `campaign_setup_answers` table.
3. **`setup.generate_world()`** uses all answers collected so far to generate the starting zone, a handful of neighbours as stubs, a big-bad archetype instance, initial resident NPCs with cross-zone backstories, and the pre-history events that populate the event log before play begins.
4. **`setup.mark_ready()`** flips the campaign from "setup" to "running", sets `campaign_hour = 0`, and emits a `campaign.started` event. All subsequent play runs against the generated world.

The setup phase is complete when `mark_ready` fires. From that point the normal tool surface takes over.

## Content: setup questions

```yaml
# content/campaign/setup_questions.yaml

- id: starting_biome
  prompt: "What kind of land does the adventure begin in?"
  options: [forest, plains, mountains, desert, coast, city, dungeon]
  or_free_text: true

- id: enemy_preference
  prompt: "What kinds of threats should feature most? (pick as many as you like)"
  options: [humanoid_raiders, undead, beasts, outsiders, political_intrigue, cults, other]
  multi: true

- id: tone
  prompt: "Grim and gritty, balanced, or swashbuckling heroic?"
  options: [grim, balanced, heroic]

- id: party_size
  prompt: "Starting alone, with a companion, or in a small party?"
  options: [solo, one_companion, small_party]

- id: companion_kind
  prompt: "What sort of companion would you like?"
  options: [warrior, scholar, rogue, pet_animal, none]
  conditional: { party_size: [one_companion, small_party] }

- id: big_bad_flavor
  prompt: "What flavour of primary antagonist should the campaign build toward?"
  options: [conqueror, corruptor, awakened_ancient, hidden_manipulator, devourer]
  or_free_text: true

- id: pacing
  prompt: "Sessions where things are calm and exploratory, or constant pressure?"
  options: [exploratory, mixed, pressured]
```

Questions are **ordered** (the agent walks them top to bottom) and can be **conditional** on previous answers (e.g. `companion_kind` only asks if the player chose to have a companion).

`or_free_text: true` means the player may answer outside the option list; the world-generator treats this as a hint rather than a constrained choice.

## Pre-history generation

`setup.generate_world()` does several things in one transaction:

### Starting zone + neighbours

- Picks the starting `zones.biome` from `starting_biome` answer.
- Creates the starting zone with `kind` appropriate to the biome (`wilderness` for forest/plains/mountains/desert/coast, `settlement` for city, `dungeon` for dungeon).
- Creates 2–5 neighbour zones as **stubs** (name, biome, kind, size, plus connections back). These are not fully generated — that happens on first visit.
- Inserts `character_zone_knowledge` rows: `visited` for the starting zone, `rumored` for the neighbours.

### Big bad

- Picks a big-bad archetype seeded by `big_bad_flavor`.
- Generates the big bad as an NPC via `npc.generate`. They're placed in a zone the player hasn't been to (often far from the starting zone; sometimes in a neighbour for quicker narrative escalation).
- Synthesizes **deeper backstory** than a normal NPC — 6 to 10 pre-history events — describing how they came to power, past crimes, allies, and lingering threats. These events seed future plot threads.

### Starting NPCs

- Full-generates the starting zone (landmarks, NPCs) using the zone template for the picked biome.
- Each generated NPC gets 3–5 backstory events, with reconciliation preferring the big bad or each other as participants.

### Pre-history events

All backstory events live in the [event log](history-log.md) with **negative `campaign_hour`** values, representing "before the campaign began." The oldest events (the big bad's origin) might be decades-negative; recent ones (the raid that scared the starting village) might be weeks-negative.

This gives the DM agent a rich backdrop to draw on — every NPC, every landmark, every rumored far-off place has some connective tissue in the log.

### Player character

The player character is created earlier in the setup flow (or is already present, depending on the agent's UX) via a standard `character.create` pathway from the player's chosen class/species/stats. The setup phase doesn't design the player character's mechanics — it just asks about tone and party shape and builds the world around them.

## Schema

Setup adds one small table:

```
campaign_setup_answers(
    question_id     TEXT PRIMARY KEY,
    answer          TEXT,          -- JSON; array for multi-select, string otherwise
    answered_at     INTEGER        -- real time; campaign_hour is still 0
)
```

Plus a single row in a `campaign_state` table tracking whether the campaign is in `setup` or `running`:

```
campaign_state(
    id              INTEGER PRIMARY KEY CHECK (id = 1),   -- singleton
    phase           TEXT NOT NULL,                        -- 'setup' | 'running'
    started_at      INTEGER?,                             -- real time of mark_ready
    player_character_id FK?
)
```

`CHECK (id = 1)` enforces a single row. Every tool that reads or writes campaign state can trust there's exactly one.

## Tools

| Tool                                              | Effect                                                              |
|---------------------------------------------------|---------------------------------------------------------------------|
| `setup.new_campaign()`                            | Create empty DB + schema; return setup questions.                   |
| `setup.answer(question_id, answer)`               | Record a player response to one question.                           |
| `setup.get_answers()`                             | Return all answers recorded so far (agent uses for branching).      |
| `setup.generate_world()`                          | Build starting zone + neighbours + big bad + starting NPCs + pre-history events. |
| `setup.mark_ready(player_character_id)`           | Flip the campaign to `running`, `campaign_hour = 0`, emit `campaign.started`. |

## Why this matters

The setup phase is cheap to author (one YAML file, a short ordered list of questions) and cheap to run (a few seconds of generator work in one transaction). It transforms the generic "you awake in a forest..." opening into something that reflects the player's tastes and gives the DM agent a world to narrate rather than a blank page.

Combine it with [lazy generation](world.md#lazy-generation) and [the reconciliation pass](npcs.md#reconciliation), and every campaign gets its own identity while the ongoing world continues to build on itself session by session.
