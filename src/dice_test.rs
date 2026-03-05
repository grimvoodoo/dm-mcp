//! Tests for dice rolling functionality

use crate::dice::{roll_die, roll_multiple_dice, parse_dice_type, roll_dice, RollRequest};

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