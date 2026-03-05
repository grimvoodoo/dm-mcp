use std::env;
use std::error::Error;

mod dice;
mod stdio;
mod httpstream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    
    if args.len() < 2 {
        eprintln!("Usage: {} [stdio|httpstream]", args[0]);
        std::process::exit(1);
    }
    
    match args[1].as_str() {
        "stdio" => {
            println!("Dice rolling service running in stdio mode");
            println!("Supported dice types: d4, d6, d8, d10, d12, d20, d100");
            println!("Custom ranges: e.g., 11-52");
            stdio::run_stdio()?;
        }
        "httpstream" => {
            println!("Dice rolling service running in httpstream mode");
            println!("Supported dice types: d4, d6, d8, d10, d12, d20, d100");
            println!("Custom ranges: e.g., 11-52");
            httpstream::run_httpstream(3000).await?;
        }
        _ => {
            eprintln!("Unknown transport type. Use 'stdio' or 'httpstream'");
            std::process::exit(1);
        }
    }
    
    Ok(())
}