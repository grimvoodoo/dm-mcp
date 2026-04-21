# NPC generation

The world feels alive because new NPCs carry pre-existing history — crimes committed, relationships formed, agendas being pursued — and that history is woven into the campaign that's already in progress. This is the single feature that most strongly differentiates a coherent world from a procedurally-stitched one.

## One table for every character

NPCs use the exact same schema as the player and companions. See [characters](characters.md) for the full model. The only schema addition specific to NPC generation is:

```
characters.plans    TEXT?    -- prose: current agenda, motivations, goals
```

Everything else — stats, HP, AC, proficiencies, ideology, loyalty, role — already exists on `characters`. No NPC-specific table.

**No `character_relationships` table.** Relationships are **implicit via event co-participation**: if two characters appear together in a `history.relationship.family` event, they're related. Same mechanism as recognition. See [history log](history-log.md).

## Archetypes

Archetypes describe **role + current faction**, not species essence. `orc_raider` is an orc who is currently raiding; `orc_merchant` is the same species in a completely different context. See [ideology and the anti-evil-races rule](#non-violent-resolution-first-class) below — this is a hard content-authoring rule.

Archetype YAML lives in `content/npcs/archetypes/*.yaml`:

```yaml
# content/npcs/archetypes/orc_raider.yaml
id: orc_raider
species: orc
role_hint: enemy                    # player | companion | friendly | enemy | neutral
typical_age_years: [18, 45]

stats:                              # rolled within each range
  str: [14, 18]
  dex: [10, 14]
  con: [14, 18]
  int: [6, 10]
  wis: [8, 12]
  cha: [6, 10]

hp_formula: "3d8 + con_mod*3"
ac_base: 13
speed_ft: 30

proficiencies:
  - { name: 'save:str',      proficient: true }
  - { name: 'save:con',      proficient: true }
  - { name: 'intimidation',  proficient: true }
  - { name: 'survival',      proficient: true }
  - { name: 'greataxe',      proficient: true }

loadout:                            # each entry rolled independently
  - { base_kind: greataxe,      chance: 0.7, material: iron,    material_tier: 1 }
  - { base_kind: handaxe,       chance: 0.5, material: iron,    material_tier: 1 }
  - { base_kind: leather_armor, chance: 0.9, material: leather, material_tier: 1, equip: chest }
  - { base_kind: gold,          chance: 1.0, quantity: "2d6" }

plan_pool:                          # one picked at generation
  - "Raiding nearby settlements for loot and captives"
  - "Avenging the death of a clanmate killed by settlers"
  - "Seeking a lost tribal relic taken during a settler raid"
  - "Earning honour for their warband"

ideology_pool:                      # one picked at generation
  - "Strength is the only virtue worth respecting. The weak exist to be ruled."
  - "Loyal only to the warband, suspicious of outsiders, honours debts in blood."

social:
  can_parley: true                  # default true
  parley_requires: []               # [] = plain speech works
hostile_triggers:
  - "Encounter their warband in raided territory; strangers are presumed spies"

peace_hooks:                        # prose; LLM uses when framing negotiation
  - "Accepts a truce if offered blood-debt payment (3+ gold per clanmate lost)"
  - "Will side-swap if shown convincing evidence of their war-chief's treachery"
  - "Backs down if the party yields with dignity and leaves territory"
  - "Can be recruited after defeating their leader in honourable single combat"

backstory_hooks:                    # 3-5 rolled at generation; slots filled by reconciliation
  - "Raided ${settlement} ${years_ago} years ago, after ${oppressor} ${past_wrong_against_orcs}"
  - "Killed ${enemy_archetype} in single combat over ${cause}"
  - "Fled from ${stronger_foe} after losing the duel"
  - "Was exiled from ${tribe} for breaking a sacred law"
  - "Carries a trophy taken from ${victim} at ${place}"
```

Name pools live in `content/npcs/name_pools/*.yaml` indexed by species or culture:

```yaml
orc:
  first: [Grog, Zhar, Urgash, Maznok, ...]
  last: [Skullcrusher, Ironfist, Bonebreaker, ...]
human_northern:
  first: [Bjorn, Inga, Sigrid, ...]
```

## Generation procedure

`npc.generate(archetype, zone_id?, role_override?)`:

1. **Load context** — the current campaign state: active NPCs, zones visited, unresolved plot threads, the big bad's moves, items the player is seeking. This is the input to the reconciliation pass.
2. **Roll stats** in the archetype's ranges.
3. **Compute HP** from the formula, using the rolled CON modifier.
4. **Create the character row** with rolled stats, pool-picked ideology and plans, archetype id as `class_or_archetype`, `zone_id`, `status='alive'`, `role` from `role_hint` or the override.
5. **Insert proficiencies** from the archetype.
6. **Roll the loadout**: for each item entry, roll against `chance`; on success, create an `items` row with `holder_character_id = new character` and optional `equipped_slot`.
7. **Pick a name** from the species/culture pool.
8. **Synthesize backstory events** — 3 to 5, using the archetype's `backstory_hooks`. For each hook:
   - Fill `${slot}` placeholders via the reconciliation pass (below).
   - Pick a `campaign_hour` in the past — negative, representing years before campaign start.
   - Insert a `history.backstory` event with this character and any referenced entities as participants.

All of this runs in a single transaction.

## Reconciliation

The reconciliation pass is the mechanism that makes lazily-generated content feel like part of a coherent world. When `${slot}` placeholders in backstory hooks (or zone landmarks, or generated items) need filling, the pass prefers **existing entities** over inventing new ones:

**Priority order for filling a slot:**

1. An entity the player has **directly interacted with**. Highest narrative weight — creates recognition payoff when the player next encounters the NPC whose backstory references someone they've met.
2. An entity the player has **heard of but not met**. Builds anticipation; confirms earlier rumors.
3. An entity that **exists but no-one has encountered yet**. Enriches the world without a direct hook.
4. **Generate a new stub entity**. Last resort — a long-dead ancestor, a distant village that may never be visited. Keep these minimal.

Because the pass runs for zone generation, NPC generation, item placement, and any other mid-campaign content creation, the reconciliation logic is shared. Think of it as: *whenever the world grows, look for ways to tie the growth into what's already there.*

This is why a blacksmith in village A might have a backstory that references an orc raid on village B — even if the player has only ever visited A. When the player later travels to B and meets its survivors, they already share a backstory element with someone the player knows.

## Zone generation calls `npc.generate`

Zone templates specify NPC slots, each pointing at an archetype:

```yaml
# content/world/zone_templates/village.yaml
village_small:
  npc_slots:
    - { archetype: village_elder,    count: 1 }
    - { archetype: tavern_keeper,    count: 1 }
    - { archetype: blacksmith,       count: [0, 1] }
    - { archetype: villager,         count: [4, 8] }
    - { archetype: village_guard,    count: [1, 3] }
```

The zone generator rolls each slot's count and calls `npc.generate(archetype, zone_id=this_zone)` for each. Reconciliation runs across zones, so an NPC in one zone can reference history with an NPC in another zone.

## Non-violent resolution (first-class)

**No species is "evil by default".**

Archetypes describe the role + current faction the character inhabits, not their inherent nature. Hostility is always traceable to situational cause:

- An orc raid is a raid **because of** an oppression, a stolen artefact, a blood-debt from previous conflict.
- A cultist is hostile **because** their ideology frames outsiders as prey or witnesses.
- A bandit is hostile **because** they're desperate, coerced, or hired.

Every hostility should be **resolvable without combat** in principle:

| Target                      | Peaceful path                                          |
|-----------------------------|--------------------------------------------------------|
| Sapient hostile (raider, cultist) | Negotiation, side-swap, bribery, intimidation, revelation |
| Animal-intelligence creature | Animal handling, `speak_with_animals`, food, terrain    |
| Mindless (skeleton, ooze)    | Destruction, control magic, avoidance — `can_parley: false` |

### Content-authoring rules

- **No alignment axis** in archetype YAML. No `alignment: evil`, no `tends_hostile: true`.
- **Backstory hooks carry causes, not just acts**: `"Raided ${settlement} after ${oppressor} enslaved ${relation}"` not just `"Raided ${settlement}"`. The reconciliation pass can then wire the cause to existing campaign threads.
- **Every hostile encounter** (see [encounters](encounters.md#resolution-paths)) must have at least one peaceful `resolution_path` — unless all participants are truly mindless.

### Archetype's social block

Two fields on the archetype gate the check-based peaceful paths:

```yaml
social:
  can_parley: true                    # default true; false only for mindless
  parley_requires: []                 # e.g. ['speak_with_animals'] for beasts
```

`resolve_check` of kind `persuade`/`deceive`/`intimidate` is **refused** if the target's archetype has `can_parley: false`. For `parley_requires: [...]`, the agent reads this and either satisfies the requirement (via an effect granting the required ability) or knows the plain-speech route is unavailable.

INT score drives interpretation at the edges:
- `int: [1, 3]` → mindless; typically `can_parley: false`.
- `int: [3, 7]` → animal-level; `parley_requires: ['speak_with_animals']` or requires Animal Handling.
- `int: [8+]` → sapient; ordinary speech works.

### Hostile triggers

Some archetypes have contexts that make them **reflexively hostile** — not because of who they are, but because of where they are or what they're doing:

```yaml
hostile_triggers:
  - "Anyone not wearing the cult's sigil is potential prey or witness"
  - "Outsiders encountered near ritual sites are presumed hostile"
```

When a trigger fires on first contact, it biases the initial ideology-alignment tier toward `very_misaligned`. The trigger is not unbreakable — a clever approach (disguise, false sigil, appeal to demon-lord hierarchy) can sidestep it — but it raises the DC of peaceful first contact significantly.

### Role swaps are first-class

When the player's actions reveal that an "enemy" had a legitimate grievance and a new alliance forms, `character.change_role(character_id, new_role, reason)` captures the pivot:

- Emits `npc.role_changed` with before/after role values and the narrative reason.
- The audit trail lets later queries answer "when did this NPC become an ally?"
- The full mechanic of side-swapping (villager becomes enemy after betrayal, enemy becomes companion after reconciliation) is **just a column update + an event**. No state machine.

## Recall and recognition

One generic tool covers "what does the player remember" and "what does this NPC know":

```
character.recall(character_id, {
  zone_id?,
  other_character_id?,
  other_item_id?,
  kind_prefix?,
  since_hour?,
  limit?
})
  → list of events this character participated in
    (as actor | target | witness | beneficiary),
    filtered by the optional criteria,
    newest-first (but `since_hour=<negative>` includes pre-campaign backstory).
```

"What does villager Bob know about this orc?" → `character.recall(bob.id, { other_character_id: orc.id })`. If Bob was a witness-role participant in the orc's raid-on-Ashfield backstory event, that event comes back — and the agent can narrate Bob's recognition naturally.

Same indexed query path serves both real-play recall and pre-campaign backstory recognition. The event log doesn't distinguish between "this happened in play" and "this was synthesized at NPC generation."

## Tools

| Tool                                                         | Effect                                                                |
|--------------------------------------------------------------|-----------------------------------------------------------------------|
| `npc.generate(archetype, zone_id?, role_override?)`          | Create an NPC with rolled stats, loadout, and synthesized backstory.  |
| `character.recall(character_id, filters)`                    | Events this character participated in. See above.                     |
| `character.update_plans(character_id, new_plans, source_event_id)` | Overwrite plans; emit `npc.plan_changed`.                       |
| `character.change_role(character_id, new_role, reason)`      | Side-swap; emits `npc.role_changed`.                                  |

## Deferred

- **Autonomous NPC behaviour between sessions.** Plans advancing without the player present. Post-MVP.
- **Formal faction system.** Factions are currently implicit in shared archetypes + shared backstory events. A dedicated `factions` table may be added later if queries like "which NPCs belong to the cult" become common.
- **Explicit `character_relationships` table.** Relationships are currently derived from event co-participation. A dedicated table would be a denormalisation for performance; not needed yet.
- **Dynamic alignment shifts with complex state machines.** Role change is a single column update + an event — intentionally simple. More nuanced shifts (partial alignment, reversible betrayals) can be layered via additional event kinds without schema change.
