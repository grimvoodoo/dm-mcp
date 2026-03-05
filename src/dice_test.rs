//! Tests for dice rolling functionality

use crate::dice::{roll_die, roll_multiple_dice, parse_dice_type, roll_dice, RollRequest, roll_multiple_dice_request};

#[test]
fn test_roll_die() {
    // Test each die type 10 times and ensure at least two different numbers are returned
    let die_types = vec!["d4", "d6", "d8", "d10", "d12", "d20", "d100"];
    
    for die_type in die_types {
        let mut results = Vec::new();
        for _ in 0..10 {
            let result = roll_die(die_type);
            results.push(result);
            
            // Ensure each result is within the expected range
            match die_type {
                "d4" => assert!(result >= 1 && result <= 4),
                "d6" => assert!(result >= 1 && result <= 6),
                "d8" => assert!(result >= 1 && result <= 8),
                "d10" => assert!(result >= 1 && result <= 10),
                "d12" => assert!(result >= 1 && result <= 12),
                "d20" => assert!(result >= 1 && result <= 20),
                "d100" => assert!(result >= 1 && result <= 100),
                _ => panic!("Unexpected die type"),
            }
        }
        
        // Ensure at least two different numbers were returned
        // Simplified approach: check if we have at least 2 unique values
        let mut seen_values = std::collections::HashSet::new();
        for &value in &results {
            seen_values.insert(value);
        }
        assert!(seen_values.len() >= 2, "Die {} should return at least 2 different values in 10 rolls", die_type);
    }
}

#[test]
fn test_roll_multiple_dice() {
    // Test rolling multiple dice 10 times to ensure at least two different numbers are returned
    let die_type = "d6";
    let count = 3;
    
    let mut all_results = Vec::new();
    for _ in 0..10 {
        let results = roll_multiple_dice(die_type, count);
        assert_eq!(results.len(), count);
        
        for &result in &results {
            assert!(result >= 1 && result <= 6);
            all_results.push(result);
        }
    }
    
    // Ensure at least two different numbers were returned across 10 rolls
    let mut seen_values = std::collections::HashSet::new();
    for &value in &all_results {
        seen_values.insert(value);
    }
    assert!(seen_values.len() >= 2, "Multiple dice rolling should return at least 2 different values in 10 rolls");
    
    // Verify that the total sum is within expected bounds
    let total: i32 = all_results.iter().sum();
    let min_expected = count as i32 * 1;  // Minimum possible sum (each die shows 1)
    let max_expected = count as i32 * 6;  // Maximum possible sum (each die shows 6)
    assert!(total >= min_expected && total <= max_expected,
            "Total sum {} should be between {} and {}", total, min_expected, max_expected);
}

#[test]
fn test_parse_dice_type() {
    assert_eq!(parse_dice_type("d4").unwrap(), 4);
    assert_eq!(parse_dice_type("d6").unwrap(), 6);
    assert_eq!(parse_dice_type("d8").unwrap(), 8);
    assert_eq!(parse_dice_type("d10").unwrap(), 10);
    assert_eq!(parse_dice_type("d12").unwrap(), 12);
    assert_eq!(parse_dice_type("d20").unwrap(), 20);
    assert_eq!(parse_dice_type("d100").unwrap(), 100);
    
    assert_eq!(parse_dice_type("11-52").unwrap(), 20); // This is the default for non-dice strings
    
    assert!(parse_dice_type("invalid").is_err());
    assert!(parse_dice_type("d3").is_err());
}

#[test]
fn test_roll_multiple_dice_request() {
    let result = roll_multiple_dice_request("d6", 3);
    assert_eq!(result.results.as_ref().unwrap().len(), 3);
    for &value in result.results.as_ref().unwrap() {
        assert!(value >= 1 && value <= 6);
    }
    assert!(result.result >= 3 && result.result <= 18); // 3 dice, each between 1-6
    assert_eq!(result.dice, "3d6");
}

#[test]
fn test_roll_dice_multiple() {
    // Test the roll_dice function with multiple dice format like "3d6"
    let result = roll_dice("3d6");
    assert!(result.results.is_some());
    assert_eq!(result.results.as_ref().unwrap().len(), 3);
    for &value in result.results.as_ref().unwrap() {
        assert!(value >= 1 && value <= 6);
    }
    assert!(result.result >= 3 && result.result <= 18); // 3 dice, each between 1-6
    assert_eq!(result.dice, "3d6");
}

#[test]
fn test_roll_dice_multiple_different_counts() {
    // Test with different dice counts
    let result = roll_dice("5d20");
    assert!(result.results.is_some());
    assert_eq!(result.results.as_ref().unwrap().len(), 5);
    for &value in result.results.as_ref().unwrap() {
        assert!(value >= 1 && value <= 20);
    }
    assert!(result.result >= 5 && result.result <= 100); // 5 dice, each between 1-20
    assert_eq!(result.dice, "5d20");
}

#[test]
fn test_roll_dice_single() {
    // Test that single die rolling still works (backward compatibility)
    let result = roll_dice("d6");
    assert!(result.results.is_none());
    assert!(result.result >= 1 && result.result <= 6);
    assert_eq!(result.dice, "d6");
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