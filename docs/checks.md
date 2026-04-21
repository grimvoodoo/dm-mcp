# Checks, effects & conditions

`resolve_check` is the most frequently-called tool. It must be fast, deterministic, auditable, and flexible enough to cover every kind of d20-inspired check — skill checks, saving throws, attack rolls, ability checks, command/obedience, social interactions.

## The big picture

A check composes four inputs, in order:

1. **The character's base numbers** — ability scores, proficiency bonus, level.
2. **Proficiency in the specific check's key** — skill, save, weapon, tool, or custom-growth skill like a pet's "bite."
3. **Active effects** — rows in the `effects` table targeting the relevant key (e.g. *Bless* adds `1d4` to attack rolls and saves; a curse subtracts 2 from a specific stat).
4. **Active conditions** — rows in `character_conditions` whose mechanical riders (advantage/disadvantage, auto-fail, auto-crit) apply.

Plus caller-supplied **situational modifiers** passed in by the DM agent (ideology alignment, environmental hazards, social context).

## Unified proficiencies table

One table handles 5e-style skills, saving throws, weapon proficiencies, tool proficiencies, **and** pet-style growth skills:

```
character_proficiencies(
    character_id    FK,
    name            TEXT,        -- 'stealth', 'save:str', 'longsword', 'thieves-tools', 'bite'
    proficient      BOOL,
    expertise       BOOL,        -- rogue expertise: doubles proficiency bonus
    ranks           INTEGER,     -- flat additive; used for pet/custom skills
    PRIMARY KEY (character_id, name)
)
```

Naming convention for `name`:
- **Skills**: the skill name alone — `stealth`, `perception`, `persuasion`.
- **Saving throws**: prefixed — `save:str`, `save:wis`, etc.
- **Weapon proficiencies**: the base kind — `longsword`, `longbow`, `unarmed_strike`.
- **Tool proficiencies**: the tool identifier — `thieves-tools`, `smith-tools`.
- **Custom / pet skills**: any string the content authors like — `bite`, `intimidating-howl`, `enchanted-resonance`.

A rogue with expertise in Stealth: `(stealth, proficient=1, expertise=1, ranks=0)`.
A starting dog: `(bite, proficient=0, expertise=0, ranks=1)`.
A fighter's weapon training: `(longsword, proficient=1, expertise=0, ranks=0)`.

Same query path for every check.

## Effects

Temporary numerical modifiers. **Never mutate base stats.**

```
effects(
    id                       PK,
    target_character_id      FK,
    source                   TEXT,         -- 'potion:giant-strength', 'curse:rotspeak', 'spell:bless'
    target_kind              TEXT,         -- ability | ac | speed | hp_max | attack | damage | skill | save
    target_key               TEXT,         -- 'str_score', 'armor_class', 'stealth', 'save:con'
    modifier                 INTEGER,      -- signed; negative for debuffs
    dice_expr                TEXT?,        -- e.g. '1d4' for Bless; rolled per check
    start_event_id           FK,
    expires_at_hour          INTEGER?,     -- world-time expiry
    expires_after_rounds     INTEGER?,     -- combat-round expiry
    expires_on_dispel        BOOL,
    active                   BOOL
)
```

Index: `effects(target_character_id, active)` — every `resolve_check` reads this.

### Effective-value semantics

```
effective_str = str_score
              + SUM(effects.modifier
                    WHERE target_character_id = me
                      AND active = 1
                      AND target_key = 'str_score')
```

The **base** `str_score` column never changes when a potion is drunk. The effect row is inserted; when the duration ticks down or is dispelled, the row is marked `active = 0` and the base is untouched. This makes "what were my stats before the curse" a free query and lets stacking effects compose.

### `target_kind` alongside `target_key`

`target_kind` is coarse (`ability`, `ac`, `speed`, `skill`, `save`, etc.); `target_key` is specific (`str_score`, `stealth`, `save:con`). Having both lets code branch cheaply ("all ability-score effects on this character") without wildcard string matching.

### `dice_expr` for rolled bonuses

Most effects are flat integers. Some add a rolled die per check — *Bless* famously adds `1d4` to attack rolls and saving throws. The `dice_expr` column captures that; `resolve_check` rolls them as part of the check and records each roll in `payload.rolls`.

## Conditions

Named states (blinded, poisoned, paralyzed, exhaustion, encumbered, mortally_wounded) with **mechanical riders** that aren't pure numerical modifiers.

```
character_conditions(
    id                       PK,
    character_id             FK,
    condition                TEXT,         -- named: 'blinded', 'charmed', 'poisoned', 'exhaustion', ...
    severity                 INTEGER,      -- 1 for binary conditions; 1-6 for exhaustion
    source_event_id          FK,
    expires_at_hour          INTEGER?,
    expires_after_rounds     INTEGER?,
    remove_on_save           TEXT?,        -- e.g. 'save:con:dc15' — retried per round
    active                   BOOL
)
```

### Conditions are separate from effects

They overlap — both apply to a character, both have a source and an expiry. The difference is **shape of consequence**:

- Effects produce *numerical modifiers* that add into a check's total.
- Conditions produce *rule-level changes* — "disadvantage on attacks," "auto-fail STR saves," "speed penalty," "can't take actions."

Trying to express "paralyzed auto-fails STR and DEX saves" as a numerical effect ends in edge cases. Better to keep conditions as named states and have their mechanical consequences defined in content.

### Mechanical riders live in content, not code

`content/rules/conditions.yaml`:

```yaml
blinded:
  self:
    attack_rolls: disadvantage
    sight_checks: auto_fail
  against:
    attack_rolls: advantage

paralyzed:
  self:
    can_move: false
    can_act: false
    auto_fail: [save:str, save:dex]
  against:
    attack_rolls: advantage
    auto_crit_if_melee_within_5ft: true

encumbered:
  self:
    speed_penalty: -10

mortally_wounded:
  self:
    can_act: false
    hp_floor: 0
```

The MCP is opinionated about *what these conditions mean* — but the meaning is data, not code. A homebrew profile can override riders without a code change.

### Condition composition

Multiple active conditions compose. Per standard d20 rules, multiple sources of disadvantage still count as a single disadvantage (not double-disadvantage). `resolve_check` walks active conditions and deduplicates rider flags.

## The check flow

```
resolve_check(character_id, check_spec):
  char   = SELECT characters WHERE id = character_id
  effs   = SELECT * FROM effects WHERE target_character_id = ? AND active = 1
  conds  = SELECT * FROM character_conditions WHERE character_id = ? AND active = 1
  prof   = SELECT * FROM character_proficiencies
             WHERE character_id = ? AND name = check_spec.key

  # 1. Ability modifier (effective, not base)
  effective_score = char[check_spec.ability] + sum(effs where target_key = ability)
  ability_mod     = (effective_score - 10) / 2    # floor

  # 2. Proficiency contribution
  prof_contrib  = char.proficiency_bonus * (1 + prof.expertise) * prof.proficient + prof.ranks

  # 3. Effect contributions for this key
  effect_flat   = sum(effs where target_key matches check_spec.key)
  effect_dice   = roll_dice_expressions(effs where target_key matches AND dice_expr != NULL)

  # 4. Condition riders
  rider_flags   = compose_conditions(conds, check_spec.kind)  # adv, dis, auto_fail, ...
  if rider_flags.auto_fail: emit failure directly; skip the roll

  # 5. Situational modifiers from caller
  caller_mods   = check_spec.modifiers  # [{ kind, value, reason }, ...]

  # 6. Roll
  rolls = roll_d20(advantage = rider_flags.advantage,
                   disadvantage = rider_flags.disadvantage)
  total = pick_d20(rolls, rider_flags)
        + ability_mod + prof_contrib + effect_flat + effect_dice
        + sum(caller_mods.value)

  # 7. Compare to DC
  success = total >= check_spec.dc
  crit    = rolls.contains(20)  # for attack rolls; configurable
  fumble  = rolls.contains(1)

  # 8. Emit event and return
  emit event kind='check.resolve', payload={dc, total, rolls, breakdown, caller_mods, ...}
  return { total, success, crit, fumble, rolls, breakdown, event_id }
```

The full breakdown is returned to the caller and logged in the event's payload for audit.

## Check request shape

```json
{
  "character_id": 42,
  "kind": "skill_check",
  "skill": "persuasion",
  "ability": "cha",
  "target_character_id": 87,
  "dc": 15,
  "modifiers": [
    {
      "kind": "ideology_alignment",
      "value": -6,
      "reason": "cultist's demon-sacrifice goal vs. request to release captive"
    },
    {
      "kind": "hostile_trigger",
      "value": -3,
      "reason": "encountered near a ritual site"
    },
    {
      "kind": "situational",
      "value": +2,
      "reason": "player produced a forged cult sigil"
    }
  ]
}
```

### Named modifier kinds

The DM agent passes modifiers through a free `kind` string so the audit event captures intent:

| Kind                 | Typical use                                                           |
|----------------------|-----------------------------------------------------------------------|
| `ideology_alignment` | Ideology-rubric-driven modifier for social checks                     |
| `hostile_trigger`    | Archetype context that pre-biases toward hostility                    |
| `loyalty`            | Companion-obedience: loyalty-score-to-modifier translation            |
| `situational`        | Catch-all for environmental, narrative, or improvisational modifiers  |
| `cover`              | Combat cover (half/three-quarters/full)                               |
| `flanking`           | Positional tactical modifier                                          |

Every modifier lands in the check event's `payload.modifiers` so later queries ("why did Kira fail that roll?") can cite exact contributors.

## Ideology alignment

Locked rubric lives in content:

```yaml
# content/rules/ideology_alignment.yaml
very_aligned:    +3      # the ask actively furthers their goals
aligned:         +1      # compatible with their worldview
neutral:          0      # neither helps nor hurts their agenda
misaligned:      -3      # contradicts a held value
very_misaligned: -6      # directly opposes a core goal
hostile_core:   -10      # asks them to betray their deepest commitment
```

The DM agent reads the target's `ideology` (prose) and `plans` (prose), interprets the player's specific request, picks a tier, and passes the modifier. The rubric keeps magnitudes consistent across sessions.

### Composition

**Allies and companions:**
```
total = base + ability_mod + prof_bonus + ranks
      + loyalty_bonus                    # persistent relationship
      + ideology_alignment               # this specific ask
      + situational
```

**Strangers and enemies:**
```
total = base + ability_mod + prof_bonus + ranks
      + ideology_alignment
      + hostile_trigger (if applicable)
      + situational
```

Loyalty and ideology-alignment are **orthogonal**. Loyalty captures persistent relationship; ideology captures this specific ask vs. this character's goals. A loyal companion can still refuse a deeply misaligned order; a stranger with favourable alignment can cooperate on first meeting.

## Tools

| Tool                                     | Effect                                                     |
|------------------------------------------|------------------------------------------------------------|
| `resolve_check`                          | Primary check tool. Described above.                       |
| `apply_effect(character_id, source, target_kind, target_key, modifier, dice_expr?, expires_...)` | Add an active effect row; emits `effect.applied`. |
| `dispel_effect(effect_id, reason)`       | Mark effect inactive; emits `effect.expired`.              |
| `apply_condition(character_id, condition, severity, expires_..., source_event_id)` | Add condition; emits `condition.applied`. |
| `remove_condition(condition_id, reason)` | Deactivate; emits `condition.expired`.                     |
| `tick_rounds(encounter_id, rounds)`      | Decrement round-based expiries for effects and conditions of participants; expire what hits zero. Called by `combat.next_turn`. |
| `tick_hours(hours)`                      | Decrement hour-based expiries across all characters; expire what hits zero. Called by `world.travel`, `character.short_rest`, `character.long_rest`. |
