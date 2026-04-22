//! Dice notation parsing + rolling.
//!
//! Phase 3 surface (per the Roadmap E2E assertion):
//!
//! - Standard dice: `d4`, `d6`, `d8`, `d10`, `d12`, `d20`, `d100`
//! - Multi-dice:    `3d6`, `5d20` (count × sides; roll each die, return all + sum)
//! - Arbitrary range (inclusive): `11-43`, `100-200`
//!
//! The parser is deliberately strict — unknown shapes reject with an error rather than
//! silently degrade, so the DM agent sees bad input at the tool boundary. An upper bound
//! on die count / sides guards against obvious misuse (e.g. `99999d99999`).
//!
//! Rolls are emitted individually so callers can show the player "you rolled [4, 6, 5]"
//! rather than just the sum. The `total` field is the sum of all dice rolled, or the
//! single rolled value for a range.

use anyhow::{bail, Context, Result};
// `Rng` is the core trait; `RngExt` provides `random_range` (and friends) in rand 0.10.
use rand::{Rng, RngExt};
use serde::{Deserialize, Serialize};

/// Hard upper bounds. More than 100 dice per call or more than 10_000 sides per die is
/// almost certainly a bug in the caller rather than legitimate game state; reject early.
pub const MAX_DICE_COUNT: u32 = 100;
pub const MAX_DIE_SIDES: u32 = 10_000;

/// A parsed dice-notation spec, independent of any RNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiceSpec {
    /// N dice of M sides. `count = 1, sides = 20` covers the plain `d20` case too.
    Dice { count: u32, sides: u32 },
    /// Inclusive integer range: `min ..= max`.
    Range { min: i32, max: i32 },
}

/// Structured outcome of a single `dice.roll` invocation. `rolls` has one entry per die
/// rolled (length 1 for single dice and ranges); `total` is their sum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollResult {
    pub spec: String,
    pub total: i64,
    pub rolls: Vec<i64>,
}

/// Parse a dice-notation string. Trims whitespace; case-insensitive on the `d` / `D`.
pub fn parse(spec: &str) -> Result<DiceSpec> {
    let s = spec.trim();
    if s.is_empty() {
        bail!("empty dice spec");
    }

    // Range: "MIN-MAX". Must have exactly one '-' separator between two integer parts.
    // Allow negatives on either side (e.g. "-3-5" meaning -3..=5), with care for leading '-'.
    if let Some(range) = parse_range(s)? {
        return Ok(range);
    }

    // Otherwise: NdM (with N optional, defaulting to 1).
    let lower = s.to_ascii_lowercase();
    let (count_str, sides_str) = lower.split_once('d').with_context(|| {
        format!("{spec:?} is not a recognised dice spec (expected d<N>, <C>d<N>, or <MIN>-<MAX>)")
    })?;

    let count: u32 = if count_str.is_empty() {
        1
    } else {
        count_str.parse().with_context(|| {
            format!("{spec:?}: dice count {count_str:?} is not a positive integer")
        })?
    };
    let sides: u32 = sides_str
        .parse()
        .with_context(|| format!("{spec:?}: die sides {sides_str:?} is not a positive integer"))?;

    if count == 0 {
        bail!("{spec:?}: dice count must be at least 1");
    }
    if sides < 2 {
        bail!("{spec:?}: die sides must be at least 2");
    }
    if count > MAX_DICE_COUNT {
        bail!("{spec:?}: dice count {count} exceeds MAX_DICE_COUNT={MAX_DICE_COUNT}");
    }
    if sides > MAX_DIE_SIDES {
        bail!("{spec:?}: die sides {sides} exceeds MAX_DIE_SIDES={MAX_DIE_SIDES}");
    }

    Ok(DiceSpec::Dice { count, sides })
}

/// Try to parse `MIN-MAX`. Returns Ok(None) if the input doesn't look like a range at all
/// (so the caller falls through to the d-notation parser). Returns Ok(Some(..)) on a valid
/// range, and Err for an input that looks like a range but is malformed.
fn parse_range(s: &str) -> Result<Option<DiceSpec>> {
    // A range must contain at least one digit and one '-' separator between two numeric
    // parts. "d6" has no '-'. "-5" is a negative number, not a range. Use a simple heuristic:
    // find the last '-' that isn't at index 0 or preceded by 'e'/'E' (scientific notation,
    // though we don't support that) and see if both halves parse as integers.
    if !s.contains('-') || s.to_ascii_lowercase().contains('d') {
        return Ok(None);
    }

    // Walk from the right looking for a '-' that splits the string into two valid integers.
    // Starting index 1 skips a leading '-' that belongs to a negative min.
    for (idx, _) in s.char_indices().rev().filter(|(_, c)| *c == '-') {
        if idx == 0 {
            continue;
        }
        let (lhs, rhs_with_dash) = s.split_at(idx);
        let rhs = &rhs_with_dash[1..];
        if lhs.is_empty() || rhs.is_empty() {
            continue;
        }
        if let (Ok(min), Ok(max)) = (lhs.parse::<i32>(), rhs.parse::<i32>()) {
            if min >= max {
                bail!("{s:?}: range min ({min}) must be less than max ({max})");
            }
            return Ok(Some(DiceSpec::Range { min, max }));
        }
    }

    // Contains '-' but couldn't be parsed as a range with both sides integer. Let the
    // d-notation path produce the "not a recognised dice spec" error.
    Ok(None)
}

/// Roll a parsed spec using the provided RNG. Split from [`roll`] so tests can inject a
/// deterministic RNG.
pub fn roll_with<R: Rng + ?Sized>(spec: &DiceSpec, rng: &mut R) -> RollResult {
    let (rolls, label) = match spec {
        DiceSpec::Dice { count, sides } => {
            let rolls: Vec<i64> = (0..*count)
                .map(|_| rng.random_range(1..=*sides as i64))
                .collect();
            let label = if *count == 1 {
                format!("d{sides}")
            } else {
                format!("{count}d{sides}")
            };
            (rolls, label)
        }
        DiceSpec::Range { min, max } => {
            // Inclusive on both ends — random_range with a RangeInclusive is inclusive.
            let v = rng.random_range(*min as i64..=*max as i64);
            (vec![v], format!("{min}-{max}"))
        }
    };
    let total = rolls.iter().sum::<i64>();
    RollResult {
        spec: label,
        total,
        rolls,
    }
}

/// Parse a spec string and roll it with the thread-local RNG.
pub fn roll(spec: &str) -> Result<RollResult> {
    let parsed = parse(spec)?;
    let mut rng = rand::rng();
    Ok(roll_with(&parsed, &mut rng))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parser ────────────────────────────────────────────────────────────────

    #[test]
    fn parses_standard_dice() {
        for (input, expected_sides) in [
            ("d4", 4u32),
            ("d6", 6),
            ("d8", 8),
            ("d10", 10),
            ("d12", 12),
            ("d20", 20),
            ("d100", 100),
        ] {
            let got = parse(input).expect(input);
            assert_eq!(
                got,
                DiceSpec::Dice {
                    count: 1,
                    sides: expected_sides
                },
                "{input} should parse to 1d{expected_sides}"
            );
        }
    }

    #[test]
    fn parses_case_insensitive_d() {
        assert_eq!(
            parse("D20").unwrap(),
            DiceSpec::Dice {
                count: 1,
                sides: 20
            }
        );
        assert_eq!(parse("3D6").unwrap(), DiceSpec::Dice { count: 3, sides: 6 });
    }

    #[test]
    fn parses_multi_dice() {
        assert_eq!(parse("3d6").unwrap(), DiceSpec::Dice { count: 3, sides: 6 });
        assert_eq!(
            parse("5d20").unwrap(),
            DiceSpec::Dice {
                count: 5,
                sides: 20
            }
        );
    }

    #[test]
    fn parses_ranges() {
        assert_eq!(
            parse("11-43").unwrap(),
            DiceSpec::Range { min: 11, max: 43 }
        );
        assert_eq!(
            parse("1-100").unwrap(),
            DiceSpec::Range { min: 1, max: 100 }
        );
    }

    #[test]
    fn parses_negative_range_min() {
        assert_eq!(parse("-3-5").unwrap(), DiceSpec::Range { min: -3, max: 5 });
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(
            parse("  d20  ").unwrap(),
            DiceSpec::Dice {
                count: 1,
                sides: 20
            }
        );
    }

    #[test]
    fn rejects_empty() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn rejects_zero_count_or_sides() {
        assert!(parse("0d6").is_err());
        assert!(parse("d0").is_err());
        assert!(parse("d1").is_err(), "d1 is meaningless (always 1)");
    }

    #[test]
    fn rejects_overflow() {
        assert!(parse(&format!("{}d6", MAX_DICE_COUNT + 1)).is_err());
        assert!(parse(&format!("d{}", MAX_DIE_SIDES + 1)).is_err());
    }

    #[test]
    fn rejects_inverted_range() {
        let err = parse("10-5").expect_err("inverted range should fail");
        assert!(format!("{err:#}").contains("min"));
    }

    #[test]
    fn rejects_bogus_input() {
        assert!(parse("hello").is_err());
        assert!(parse("d").is_err());
        assert!(parse("3d").is_err());
        assert!(parse("3dabc").is_err());
        assert!(parse("-").is_err());
        assert!(parse("--").is_err());
    }

    // ── Rolling ───────────────────────────────────────────────────────────────

    #[test]
    fn d20_is_in_range() {
        for _ in 0..50 {
            let r = roll("d20").unwrap();
            assert_eq!(r.rolls.len(), 1);
            assert!(r.total >= 1 && r.total <= 20, "got {}", r.total);
            assert_eq!(r.total, r.rolls[0]);
        }
    }

    #[test]
    fn multi_dice_sum_matches_rolls() {
        for _ in 0..50 {
            let r = roll("3d6").unwrap();
            assert_eq!(r.rolls.len(), 3);
            for v in &r.rolls {
                assert!(*v >= 1 && *v <= 6, "{v} out of 1..=6");
            }
            assert_eq!(r.total, r.rolls.iter().sum::<i64>());
        }
    }

    #[test]
    fn range_is_inclusive_both_ends() {
        let mut saw_min = false;
        let mut saw_max = false;
        for _ in 0..500 {
            let r = roll("11-43").unwrap();
            assert_eq!(r.rolls.len(), 1);
            assert!(r.total >= 11 && r.total <= 43, "got {}", r.total);
            if r.total == 11 {
                saw_min = true;
            }
            if r.total == 43 {
                saw_max = true;
            }
        }
        // Over 500 samples in a range of 33, each endpoint has P(miss) ≈ (32/33)^500 ≈ 2e-7.
        // Flake risk is negligible; if it does flake the error will explain.
        assert!(
            saw_min,
            "500 samples never hit the min — RNG or parser broken?"
        );
        assert!(saw_max, "500 samples never hit the max");
    }
}
