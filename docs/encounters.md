# Encounters & combat

An **encounter** is the narrative container for a situation that takes the player's focus: a bandit ambush, a negotiation with a merchant prince, a descent into a dungeon level, a puzzle room, a pursuit.

Encounters have **goals, not kill quotas**. XP is awarded when the goal is resolved, regardless of how. Combat is one resolution path among several — stealth, negotiation, side-swapping, and clever bypasses all earn the encounter's XP budget.

## Schema

```
encounters(
    id                         PK,
    zone_id                    FK,
    name                       TEXT,
    goal                       TEXT,           -- content key or free-text
    estimated_duration_hours   INTEGER,
    xp_budget                  INTEGER,        -- authored; baseline award on goal completion
    status                     TEXT,           -- active | goal_completed | abandoned | failed

    -- Combat state inline; NULL outside combat
    in_combat                  BOOL,
    current_round              INTEGER?,
    turn_index                 INTEGER?,       -- index into initiative-sorted participants

    started_at_hour            INTEGER,
    ended_at_hour              INTEGER?
)

encounter_participants(
    encounter_id               FK,
    character_id               FK,
    side                       TEXT,           -- player_side | hostile | neutral | ally
    initiative                 INTEGER?,       -- rolled at combat start; NULL outside combat
    has_acted_this_round       BOOL,
    PRIMARY KEY (encounter_id, character_id)
)
```

## Key decisions

### Goal, not participants, drives XP

Encounter XP fires on `encounter.goal_completed`, carrying the full `xp_budget` regardless of whether the player fought, negotiated, snuck past, or redirected the threat. Two corollaries:

- **"Kill XP" does not exist.** No event kind awards XP for defeating a specific enemy. A goblin chief who's part of an encounter contributes to the XP budget authorship; killing them doesn't fire XP on its own.
- **Bypass is legitimate.** Stealth past the skeletons earns the same XP as fighting them. The encounter's goal — "retrieve the item from the tower" — is what the player is rewarded for.

Paired with discretionary `xp.bonus` awards from the DM for clever play, this keeps player motivation aligned with problem-solving rather than combat grinding.

### Combat is a mode of the encounter, not a separate subsystem

The `in_combat`, `current_round`, and `turn_index` columns live directly on `encounters`. Initiative lives on `encounter_participants`. There are no `combat_encounters` or `combat_participants` tables.

This is deliberate:
- Same encounter row serves narrative and combat purposes — no join from "in this encounter" to "in this combat."
- Combat state (initiative, round) is NULL outside combat and doesn't clutter encounters that never enter combat.
- Transitioning in and out of combat is toggling a flag, not moving rows between tables.

### Combat state is cleaned up automatically

`combat.start(new_encounter_id)` **first** auto-ends any other encounter currently flagged `in_combat=1` (calling `combat.end(other_id, reason='superseded_by_new_combat')`), **then** starts the new combat.

This guards against the DM agent forgetting to call `combat.end` before moving on narratively. Without the auto-cleanup, a random goblin from a fight four hours ago could still be "in combat" when a new fight starts. The event log captures the auto-cleanup so the audit trail is intact.

### Participants outlast combat

`encounter_participants` rows persist after combat ends. That's how goal-XP payout finds who should receive XP (every participant on `player_side` gets the budget divided appropriately), and how recognition queries work ("has this character been in an encounter with that one?").

Combat-only fields (`initiative`, `has_acted_this_round`) are NULL-ed out on `combat.end`; the row itself stays.

## Resolution paths

Every authored encounter declares its resolution paths:

```yaml
# content/world/encounters/orc_raid.yaml
orc_raid:
  participants:
    - { archetype: orc_raider,   count: "1d4+2" }
    - { archetype: orc_warchief, count: 1 }
  goal: "Resolve the conflict between the raiders and the villagers"
  estimated_duration_hours: 2
  xp_budget: 300
  encounter_tags: [hostile, humanoid_raiders, social_possible]
  resolution_paths:
    - kind: combat_victory
      note: "Defeat the raiders"
    - kind: parley_to_peace
      note: "Broker peace after discovering the child-enslavement cause, mediate restitution"
    - kind: side_swap
      note: "Ally with the orcs against the villagers after learning the truth"
    - kind: redirection
      note: "Divert the raiders to a different target"
    - kind: flight
      note: "Escape / evacuate villagers without engagement"
      xp_modifier: 0.5
```

- The MCP **does not enforce** which path the player took. The DM agent calls `encounter.complete(encounter_id, path, xp_modifier?)` when the goal is reached; the event captures which path was taken in its payload.
- `xp_modifier` on a resolution path (default 1.0) multiplies the authored `xp_budget` for that path. A pure flight that abandons the villagers yields half XP; a clever redirection might yield full XP plus a discretionary bonus for cleverness.
- **Every hostile encounter must have at least one peaceful resolution path.** This is a content-authoring rule — see [IP & licensing](ip-and-licensing.md) and [NPC generation](npcs.md#non-violent-resolution). The exception is truly mindless participants (animated skeletons, undirected undead) for which combat or avoidance are the only options.

## Combat flow

### Starting combat

`combat.start(encounter_id)`:

1. Auto-end any other in-combat encounter (see above).
2. `encounters.in_combat = 1`.
3. Roll initiative for each `encounter_participants` row (d20 + character's `initiative_bonus`).
4. Sort participants by initiative descending; `turn_index = 0`; `current_round = 1`.
5. Reset `has_acted_this_round = 0` for everyone.
6. Emit `combat.start` event.

### Advancing turns

`combat.next_turn(encounter_id)`:

1. Mark the current participant's `has_acted_this_round = 1`.
2. Increment `turn_index`.
3. **If the index wraps past the last participant:**
   - `current_round += 1`.
   - `has_acted_this_round` reset to 0 for all participants.
   - Walk `effects` and `character_conditions` for this encounter's participants where `expires_after_rounds IS NOT NULL`; decrement by 1; mark any that hit 0 as inactive; emit expiry events.
4. Emit `combat.next_turn` with the new current participant.

All round-based bookkeeping happens here; nothing else ticks combat-round timers.

### Ending combat

`combat.end(encounter_id, reason?)`:

1. `encounters.in_combat = 0`; `current_round = NULL`; `turn_index = NULL`.
2. `encounter_participants.initiative = NULL`; `has_acted_this_round = 0` for all.
3. Emit `combat.end` or `combat.auto_ended` (depending on `reason`).

Combat ends explicitly via the DM calling it, or implicitly via `combat.start` on a new encounter (the auto-cleanup path).

## Per-turn action economy — deferred

The real 5e-style action economy (action, bonus action, reaction, move speed per turn) is **not enforced** by the MCP in MVP. The DM agent narrates action usage within a turn; `combat.next_turn` is the only round-level event the MCP tracks.

If enforcement becomes valuable later, it can be added as a `turn_actions_used` JSON column on `encounter_participants` without other schema impact. Deferring avoids forcing every combat tool to thread action-type checks through the dispatch layer for minimal MVP benefit.

## Tools

| Tool                                                     | Effect                                                                |
|----------------------------------------------------------|-----------------------------------------------------------------------|
| `encounter.create(zone_id, goal, participants, xp_budget, estimated_duration_hours, resolution_paths)` | Create an encounter; usually called by the zone/encounter generator. |
| `encounter.get(encounter_id)`                            | Full state: participants, status, combat mode, round, turn.           |
| `encounter.complete(encounter_id, path, xp_modifier?)`   | Mark status `goal_completed`; fire XP to `player_side` participants. Advance world clock by `estimated_duration_hours` or a DM-provided delta. |
| `encounter.abandon(encounter_id, reason)`                | Mark status `abandoned`; no XP; advance world clock by elapsed.       |
| `encounter.fail(encounter_id, reason)`                   | Mark status `failed`; no XP; advance world clock by elapsed.          |
| `encounter.add_participant(encounter_id, character_id, side)` | Add a character (reinforcements, bystander entering the fray). |
| `encounter.remove_participant(encounter_id, character_id, reason)` | Remove (fled, captured, unconscious out of fight). |
| `combat.start(encounter_id)`                             | Enter combat mode. Auto-ends any other in-combat encounter.           |
| `combat.next_turn(encounter_id)`                         | Advance initiative pointer; tick round timers at round boundary.      |
| `combat.end(encounter_id, reason?)`                      | Exit combat mode.                                                     |
| `combat.apply_damage(character_id, amount, damage_type, source?)` | Reduce `hp_current`; trigger [death flow](characters.md#death) if HP hits 0. |
| `combat.apply_healing(character_id, amount, source?)`    | Increase `hp_current` up to `hp_max`; if `mortally_wounded`, clear condition and reset death-save counters. |
| `encounter.award_bonus_xp(character_id, xp, reason)`     | DM-discretionary `xp.bonus` event.                                    |
