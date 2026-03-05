use std::io::{self, BufRead, BufReader, Write};

use crate::dice::{roll_dice, RollRequest};

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
        
        // Parse the input as a dice roll request
        let roll_request = RollRequest {
            dice: input.to_string(),
        };
        
        // Perform the dice roll
        let roll_result = roll_dice(&roll_request.dice);
                
        // Send the result back to the client
        let response = serde_json::to_string(&roll_result)?;
        writeln!(stdout, "{}", response)?;
        stdout.flush()?;
    }
    
    Ok(())
}