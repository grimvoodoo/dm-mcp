use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct RollRequest {
    pub dice: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RollResult {
    pub result: i32,
    pub dice: String,
}

pub fn roll_die(sides: u32) -> i32 {
    let mut rng = rand::thread_rng();
    rng.gen_range(1..=sides) as i32
}

pub fn roll_multiple_dice(dice_type: &str, count: usize) -> Vec<i32> {
    (0..count)
        .map(|_| roll_die(parse_dice_type(dice_type)))
        .collect()
}

fn parse_dice_type(dice_type: &str) -> u32 {
    if dice_type.starts_with('d') {
        dice_type[1..].parse().unwrap_or(20)
    } else {
        dice_type.parse().unwrap_or(20)
    }
}

pub fn roll_dice(input: &str) -> RollResult {
    let input = input.trim();
    
    // Handle custom range (e.g., "11-52")
    if input.contains('-') {
        let parts: Vec<&str> = input.split('-').collect();
        if parts.len() == 2 {
            let min: u32 = parts[0].parse().unwrap_or(1);
            let max: u32 = parts[1].parse().unwrap_or(100);
            if min < max {
                let mut rng = rand::thread_rng();
                let result = rng.gen_range(min..=max) as i32;
                return RollResult {
                    result,
                    dice: input.to_string(),
                };
            }
        }
    }
    
    // Handle standard dice types (e.g., "d6", "20", "d100")
    let sides = parse_dice_type(input);
    let result = roll_die(sides);
    
    RollResult {
        result,
        dice: input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roll_die() {
        let result = roll_die(6);
        assert!(result >= 1 && result <= 6);
    }

    #[test]
    fn test_roll_multiple_dice() {
        let results = roll_multiple_dice("d6", 3);
        assert_eq!(results.len(), 3);
        for &result in &results {
            assert!(result >= 1 && result <= 6);
        }
    }

    #[test]
    fn test_roll_dice_standard() {
        let result = roll_dice("d6");
        assert!(result.result >= 1 && result.result <= 6);
        assert_eq!(result.dice, "d6");
    }

    #[test]
    fn test_roll_dice_custom_range() {
        let result = roll_dice("11-52");
        assert!(result.result >= 11 && result.result <= 52);
        assert_eq!(result.dice, "11-52");
    }
}