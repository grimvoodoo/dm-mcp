use std::io::{self, BufRead, BufReader, Write};

use crate::dice::{roll_dice, roll_multiple_dice_request, RollRequest};

pub fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut stdout = stdout.lock();
    
    
    loop {
        let mut input = String::new();
        let bytes_read = reader.read_line(&mut input)?;
        
        if bytes_read == 0 {
            break; // EOF reached
        }
        
        let input = input.trim();
        
        if input.is_empty() {
            continue;
        }
        
        // Check if this is a multiple dice request (format like "2d6" or "5d20")
        let roll_result;
        if input.contains('d') || input.contains('D') {
            // Handle multiple dice rolling like "2d6" or "5d20"
            let parts: Vec<&str> = input.split('d').collect();
            if parts.len() == 2 {
                let count: usize = parts[0].parse().unwrap_or(1);
                let dice_type = parts[1];
                roll_result = roll_multiple_dice_request(dice_type, count);
            } else {
                // Fallback to single die rolling
                let roll_request = RollRequest {
                    dice: input.to_string(),
                    count: None,
                };
                roll_result = roll_dice(&roll_request.dice);
            }
        } else {
            // Handle single die rolling
            let roll_request = RollRequest {
                dice: input.to_string(),
                count: None,
            };
            roll_result = roll_dice(&roll_request.dice);
        }
                
        // Send the result back to the client
        let response = serde_json::to_string(&roll_result)?;
        writeln!(stdout, "{}", response)?;
        stdout.flush()?;
    }
    
    Ok(())
}