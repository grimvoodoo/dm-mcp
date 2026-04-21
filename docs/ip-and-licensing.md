# IP & licensing

This project borrows mechanical shape from decades of tabletop-RPG design — six ability scores, advantage/disadvantage, d20 resolution, hit points, armor class. That's fine: **game mechanics and formulas are not copyrightable**. What *is* protected is the specific text of published products and the distinctive proper nouns, and this project must not infringe on either.

The project is **released free**. Profit-off-someone-else's-IP is not the risk; cease-and-desist demands and takedown requests are. The rules below are written to keep the project defensible.

## Hard rules

### Content sources

**OK to draw from:**

- **5.1 SRD (CC-BY-4.0).** Wizards of the Coast released the 5.1 System Reference Document under Creative Commons Attribution in 2023. Mechanical content (ability scores, proficiency bonus, advantage/disadvantage, standard conditions, damage types, saving throws, most spells in the SRD, most monsters in the SRD) is reusable with attribution.
- **ORC-licensed material.** The Open RPG Creative License (Paizo, Kobold Press, and other contributors) covers a growing library of mechanical content, monsters, and items.
- **Public-domain mythology, folklore, and history.** Dragons, elves, wolves, graveyards, medieval taverns — all fine.
- **Original writing.** Anything you write yourself.

**Not OK to copy from:**

- **Any WotC product book** other than the 5.1 SRD. This includes the Player's Handbook, Dungeon Master's Guide, Monster Manual, Xanathar's Guide, Tasha's Cauldron, Fizban's Treasury, Mordenkainen's Monsters of the Multiverse, and every campaign setting book. WotC owns that text.
- **Even mechanically-identical spells or items** if the text is lifted from a non-SRD book. Stat blocks, flavor paragraphs, item descriptions — all copyrighted.
- **Distinctive WotC proper nouns.** Mind Flayer, Beholder, Githyanki, Githzerai, Displacer Beast, Umber Hulk, Carrion Crawler, Slaad, Kuo-toa, Modron, Tarrasque (outside the SRD), Yuan-ti (outside the SRD), named Forgotten Realms or Greyhawk deities, specific named NPCs. When in doubt, assume it's a WotC-specific name and pick an alternative.

### Branding

- **Never use** "D&D", "Dungeons & Dragons", "5e", "5th Edition", or any WotC product name in the README, documentation, code comments, user-facing strings, error messages, container image labels, or commit messages.
- **Use instead** "d20-inspired", "core rules profile", or "d20-style".
- The name `dm-mcp` is generic enough ("Dungeon Master" is pre-WotC English; "MCP" is the Model Context Protocol).

### Attribution for SRD-derived content

The CC-BY-4.0 license requires attribution. When content files include something directly derived from the SRD, cite it in a comment so the attribution is clear:

```yaml
# content/items/bases/weapons.yaml
longsword:
  # Derived from SRD 5.1 (CC-BY-4.0) — "Longsword" weapon entry
  damage: 1d8
  damage_type: slashing
  weight_lb: 3
  base_value_gp: 15
```

A single `ATTRIBUTIONS.md` in the repo root (or at the content directory root) aggregates per-file attributions for easy compliance review.

## Why this is enough

Rules and mechanics aren't copyrightable. A game with six ability scores called STR/DEX/CON/INT/WIS/CHA, a proficiency bonus that scales with level, and attack rolls against armor class is mechanically indistinguishable from 5e — and fully legitimate. What WotC owns is the *expression* (the words in their books) and the *marks* (their brand, specific product names, distinctive creature names).

Staying firmly in SRD/ORC/original territory avoids the expression side. Dropping WotC branding avoids the marks side. The result is a project that can run solo RPG campaigns in a 5e-*shaped* system without being a 5e *product*.

## Authoring workflow

When writing or reviewing a content file:

1. **Did you copy any prose from a WotC book?** If yes, rewrite in your own words — or delete it and use a mechanical-only entry with an original description.
2. **Does it use a WotC-proprietary name?** Check against common trademark lists (Mind Flayer, Beholder, etc.). Swap for a SRD-available equivalent or invent a name.
3. **If it's SRD-derived, is the attribution cited?** Add the comment.
4. **Does the README / docs / code mention "D&D" or "5e"?** Remove.

A release checklist lives in `ATTRIBUTIONS.md` (to be written when content authoring begins). Periodic scans for WotC-proprietary terms across `content/` are part of release prep.

## Examples

**OK:**

> "A longsword — versatile, reliable, good in most hands. 1d8 slashing damage, 3 lbs, 15 gp."

**Not OK** (lifted verbatim from a WotC source):

> "This sword shows signs of careful use, its blade darkened by...".

**OK (original flavor):**

> "A tall, gaunt humanoid with webbed hands and ink-black eyes. Speaks in whispers that seem to come from all around."

**Not OK** (this creature is a WotC proprietary name and should not appear by name even if stat-described correctly):

> "A Mind Flayer — a humanoid aberration that feeds on intelligent minds..."

When in doubt: write it yourself, or cite the SRD.
