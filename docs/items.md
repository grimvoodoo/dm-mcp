# Items & inventory

## Model in one sentence

Every physical thing in the world is a row in **one** `items` table. Its base properties (damage, weight, value) come from bundled content tables keyed on `base_kind`. Per-instance variation (material, enchantments, damage, charges, quantity, location) lives on the row.

## Schema

```
items(
    id                     PK,
    base_kind              TEXT,           -- content lookup key: 'longsword', 'potion-of-healing', 'gold'
    name                   TEXT?,          -- optional custom name: 'Frostbane'
    material               TEXT?,          -- 'steel', 'glass', 'wood'; null for non-material items
    material_tier          INTEGER?,       -- +1 basic ... +5 exotic
    quality                TEXT?,          -- 'pristine', 'worn', 'broken'
    quantity               INTEGER,        -- stackables; 1 for unique items
    charges                INTEGER?,       -- e.g. wand charges; null for non-charge items
    charges_max            INTEGER?,

    -- Exactly one of the three location FKs is non-null
    holder_character_id    FK?,            -- in a character's inventory
    container_item_id      FK?,            -- nested inside another item (chest, pouch)
    zone_location_id       FK?,            -- on the ground in a zone

    equipped_slot          TEXT?,          -- 'main-hand' | 'off-hand' | 'head' | 'chest' | ...

    created_at_event_id    FK?,
    updated_at             INTEGER
)

item_enchantments(
    id                     PK,
    item_id                FK,
    enchantment_kind       TEXT,           -- content key: 'glowing', 'sharpness', 'goblinbane'
    tier                   INTEGER?,       -- scaling enchantments: +1 / +2 / +3
    charges                INTEGER?,       -- depletable enchantments
    notes                  TEXT?
)
```

Location mutex is enforced by `CHECK` constraint:

```sql
CHECK (
    ((holder_character_id IS NOT NULL) +
     (container_item_id IS NOT NULL) +
     (zone_location_id IS NOT NULL)) = 1
)
```

An item is always in exactly one place: a character's inventory, nested in another item (chest, pouch, bag of holding), or loose in a zone. `inventory_transfer` tools update the three location columns atomically.

## Key decisions

### Single `items` table, not a type catalog

There is no `item_types` table in the database. The content directory defines every `base_kind` in bundled YAML:

```yaml
# content/items/bases/weapons.yaml
longsword:
  damage: 1d8
  damage_type: slashing
  weight_lb: 3
  base_value_gp: 15
  properties: [versatile]
  slot: main-hand

# content/items/bases/consumables.yaml
potion-of-healing:
  weight_lb: 0.5
  base_value_gp: 50
  stackable: true
  charges_max: 1

# content/items/bases/general.yaml
gold:
  weight_lb: 0.02
  base_value_gp: 1
  stackable: true
```

Storing every possible longsword variant (steel longsword, glass longsword, iron longsword of sharpness, …) as a row in an `item_types` table would explode combinatorially and fight the content-is-code principle.

### Material as a column, enchantments as a satellite table

A weapon or armor instance has **exactly one material** (steel, wood, bone) — one column does the job. An instance has **zero or more enchantments** — a junction table lets enchantments stack and carry their own per-enchantment state (charges, tier).

This is the correct factoring for the common case: "a glass longsword of goblin-slaying" is `{base_kind: longsword, material: glass, material_tier: 1}` plus one row in `item_enchantments`.

### Currency is just items

No dedicated `gold`/`silver`/`copper` columns on characters. A character's gold is one row:

```
items { base_kind='gold', quantity=123, holder_character_id=player.id }
```

Transfer, theft, barter, and containerised storage (gold in a pouch in a chest) all go through the same code paths as any other item.

### Stackables via `quantity`

50 arrows is one row with `quantity=50`, not 50 rows. The item's content base kind declares `stackable: true` — the insert/merge logic consolidates matching items on pickup.

Two items are "matching" for stacking if they share `base_kind`, `material`, `material_tier`, `quality`, and have no distinct enchantments. An enchanted arrow does not stack with a mundane one of the same base kind.

### Base stats come from content, not the DB

The database answers "what is this item?" with a content key (`base_kind`). The rest of the answer — damage dice, weight, base value, slot, properties — comes from the in-memory content tables.

This keeps the database small, keeps content editable without migrations, and means every item inherits updates to its base kind at reload time.

## Composed stats

When a tool needs the **effective stats** of an item — for damage rolls, encumbrance calculations, shop pricing — it composes four inputs:

```
effective_damage     = base.damage                  -- e.g. 1d8
                     + sum(enchantment.damage_bonus_if_matches_target)

effective_weight     = base.weight_lb
                     * material.weight_multiplier
                     * quantity
                     + sum(enchantment.weight_delta)

effective_value_gp   = base.value_gp
                     * material.value_multiplier
                     * quality_multiplier
                     + sum(enchantment.value_premium)

effective_name       = name
                     ?? format(base.name, material, enchantment summaries)
```

Material and quality multipliers come from content:

```yaml
# content/items/materials.yaml
steel:       { tier: 1, weight_mult: 1.0, value_mult: 1.0, damage_bonus: +0, durability: 1.0 }
iron:        { tier: 1, weight_mult: 1.1, value_mult: 0.8, damage_bonus: +0, durability: 0.9 }
glass:       { tier: 3, weight_mult: 0.5, value_mult: 3.0, damage_bonus: +1, durability: 0.3 }
dragonbone:  { tier: 5, weight_mult: 0.6, value_mult: 8.0, damage_bonus: +3, durability: 1.5 }
```

Quality multipliers for `{pristine: 1.2, worn: 0.9, broken: 0.1}` or similar.

## Enchantments

```yaml
# content/items/enchantments.yaml
glowing:
  kind: utility
  value_premium_gp: 50
  weight_delta: 0
  effects:
    - { target_kind: misc, target_key: emits_dim_light, modifier: 1 }
  description: "Sheds dim light out to 10 feet."

sharpness:
  kind: weapon
  value_premium_gp: 200
  weight_delta: 0
  scaling:
    "+1": { damage_bonus: +1 }
    "+2": { damage_bonus: +2 }
    "+3": { damage_bonus: +3 }
  description: "Magically keen edge; adds to damage."

goblinbane:
  kind: weapon
  value_premium_gp: 150
  effects:
    - { target_kind: damage, target_key: vs_goblinoid, modifier: +2 }
  description: "Bright red runes flare when a goblin is near. Extra damage against goblinoids."
```

Enchantments can scale (`tier` on `item_enchantments`), deplete (`charges`), or be static. `effects` entries in the enchantment definition are applied dynamically at check-resolution time, not inserted into the `effects` table — they're a property of the held item, not a temporary buff on the wielder.

## Encumbrance — enforced

Weight is computed, never stored. Capacity comes from content:

```yaml
# content/rules/encumbrance.yaml
capacity_per_str:             15        # capacity = STR × 15 lb
encumbered_threshold_pct:     67        # ≥67% of capacity → encumbered
overloaded_threshold_pct:     100       # ≥100% → overloaded, cannot accept pickups
```

A character's carry readout is a single aggregation:

```sql
SELECT SUM(effective_weight(items))
FROM items
WHERE holder_character_id = ?
   OR container_item_id IN (SELECT id FROM items WHERE holder_character_id = ?)
```

(Recursive for deeply-nested containers; SQLite handles this with a CTE.)

### Enforcement

- `inventory.pickup(character_id, item_id)` and `inventory.transfer(...)` **refuse** the action if it would push the recipient over 100% capacity. Returns `{ error: 'would_overload', current_weight, capacity }`.
- Between 67% and 100% capacity, the MCP applies the `encumbered` condition (see [conditions](checks.md#conditions)) — which in content carries a `speed_penalty: -10` rider. The condition is re-evaluated whenever inventory changes.

## Barter

Items carry a computed `effective_value_gp`. The `barter.exchange` tool handles trades where the player is offering items (or gold) for other items:

```
barter.exchange(character_id, merchant_character_id, offered_item_ids, requested_item_ids)
  computes: offered_value, requested_value
  rolls: resolve_check(character_id, skill='persuasion', target=merchant,
                       dc = 10 + haggle_difficulty_from_value_ratio)
  applies: exchange_rate modifier based on roll result
           (nat 20 → best terms; nat 1 → worst terms; middle → proportional)
  executes: moves items; creates/consumes gold items as needed
  emits: social.bargain event with full breakdown
```

The check outcome determines whether the player receives fair value, a bad deal, or a bonus. The exchange respects both sides of the transaction — merchants refuse manifestly bad deals (the player-offered value must be at least some content-configured fraction of the requested value).

## Shops

Shops are not a separate table. A shop is a **landmark** (see [world](world.md)) whose `corresponds_to_zone_id` points to a small zone containing the merchant (an NPC) whose inventory **is** the shop's stock. Buying from the shop is a barter exchange with the merchant; selling to the shop is the reverse.

This keeps the world model uniform — a tavern is a zone, the innkeeper is a character, the kegs behind the bar are items held by the innkeeper. No special-purpose shop schema.

## Tools

| Tool                                                                     | Effect                                                                 |
|--------------------------------------------------------------------------|------------------------------------------------------------------------|
| `inventory.create(base_kind, holder_character_id?, zone_location_id?, container_item_id?, material?, material_tier?, quality?, quantity?, enchantments?)` | Create a new item instance; location must be specified. |
| `inventory.transfer(item_id, to_character_id?, to_container_item_id?, to_zone_location_id?, quantity?)` | Move an item (or a quantity of a stackable) to a new location. |
| `inventory.pickup(character_id, item_id, quantity?)`                     | Character picks up a zone-located item. Refuses if overloaded.         |
| `inventory.drop(character_id, item_id, quantity?)`                       | Character drops an item into their current zone.                       |
| `inventory.equip(character_id, item_id, slot)`                           | Set `equipped_slot`; validates against content's slot rules.           |
| `inventory.unequip(character_id, item_id)`                               | Clear `equipped_slot`.                                                 |
| `inventory.get(character_id)`                                            | Full inventory readout with effective stats and weight totals.         |
| `inventory.inspect(item_id)`                                             | Effective stats of one item.                                           |
| `barter.exchange(character_id, merchant_character_id, offered, requested)` | Barter flow; rolls persuasion, updates both inventories.             |
