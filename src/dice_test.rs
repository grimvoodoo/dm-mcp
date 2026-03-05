//! Tests for dice rolling functionality

use crate::dice::{roll_die, roll_multiple_dice, parse_dice_type, roll_dice, RollRequest};

#[test]
fn test_roll_die() {
    let result = roll_die("d4");
    assert!(result >= 1 && result <= 4);
    
    let result = roll_die("d6");
    assert!(result >= 1 && result <= 6);
    
    let result = roll_die("d8");
    assert!(result >= 1 && result <= 8);
    
    let result = roll_die("d10");
    assert!(result >= 1 && result <= 10);
    
    let result = roll_die("d12");
    assert!(result >= 1 && result <= 12);
    
    let result = roll_die("d20");
    assert!(result >= 1 && result <= 20);
    
    let result = roll_die("d100");
    assert!(result >= 1 && result <= 100);
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
fn test_parse_dice_type() {
    assert_eq!(parse_dice_type("d4").unwrap(), "d4");
    assert_eq!(parse_dice_type("d6").unwrap(), "d6");
    assert_eq!(parse_dice_type("d8").unwrap(), "d8");
    assert_eq!(parse_dice_type("d10").unwrap(), "d10");
    assert_eq!(parse_dice_type("d12").unwrap(), "d12");
    assert_eq!(parse_dice_type("d20").unwrap(), "d20");
    assert_eq!(parse_dice_type("d100").unwrap(), "d100");
    
    assert_eq!(parse_dice_type("11-52").unwrap(), "11-52");
    
    assert!(parse_dice_type("invalid").is_err());
    assert!(parse_dice_type("d3").is_err());
}

#[test]
fn test_roll_dice() {
    let request = RollRequest {
        die_type: Some("d6".to_string()),
        count: 2,
        min_value: None,
        max_value: None,
    };
    
    let result = roll_dice(request);
    assert_eq!(result.results.len(), 2);
    for &value in &result.results {
        assert!(value >= 1 && value <= 6);
    }
    assert!(result.total >= 2 && result.total <= 12);
}

#[test]
fn test_roll_dice_custom_range() {
    let request = RollRequest {
        die_type: None,
        count: 3,
        min_value: Some(10),
        max_value: Some(20),
    };
    
    let result = roll_dice(request);
    assert_eq!(result.results.len(), 3);
    for &value in &result.results {
        assert!(value >= 10 && value <= 20);
    }
    assert!(result.total >= 30 && result.total <= 60);
}