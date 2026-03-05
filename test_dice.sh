#!/bin/bash

echo "Testing dice rolling functionality..."

# Test D6 roll
echo '{"method":"roll","params":{"dice_type":"d6"}}' | ./target/debug/random-number-generator-mcp stdio

# Test custom range roll
echo '{"method":"roll","params":{"dice_type":"11-52"}}' | ./target/debug/random-number-generator-mcp stdio

# Test D20 roll
echo '{"method":"roll","params":{"dice_type":"d20"}}' | ./target/debug/random-number-generator-mcp stdio

echo "Test completed successfully!"