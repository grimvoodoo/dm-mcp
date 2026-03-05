# Random Number Generator MCP Server

A Rust-based MCP server designed as a toolkit for AI dungeon masters, primarily focused on dice rolling functionality.

## Features

- Support for standard dice types: D4, D6, D8, D10, D12, D20, D100
- Custom range dice rolling (e.g., 11-52)
- Multiple dice rolling (e.g., 3d6, 5d20) with individual results
- Both stdio and httpstream transport layers
- TLS support using rustls instead of OpenSSL
- Self-contained with no external dependencies

## Usage

### Running the Server

```bash
# For stdio mode
cargo run stdio

# For httpstream mode (default port 3000)
cargo run httpstream
```

### Dice Roll Formats

- Standard dice: `d4`, `d6`, `d8`, `d10`, `d12`, `d20`, `d100`
- Custom ranges: `11-52` (rolls between 11 and 52)
- Multiple dice: `3d6` (rolls 3 six-sided dice), `5d20` (rolls 5 twenty-sided dice)

### Transport Layers

#### Stdio Mode
The server accepts dice roll requests via standard input and outputs results to standard output.

#### Httpstream Mode
The server runs an HTTP API on port 3000 with a `/roll` endpoint that accepts JSON requests and returns JSON responses.

## API Endpoints

### POST /roll

Request body:
```json
{
  "dice": "d20"
}
```

Response:
```json
{
  "result": 15,
  "dice": "d20",
  "results": null
}
```

For multiple dice rolling, the response includes individual results:
```json
{
  "result": 12,
  "dice": "3d6",
  "results": [3, 4, 5]
}
```

## Building

```bash
cargo build --release
```

## Dependencies

- Rust 1.60+
- tokio
- serde
- rand
- hyper 1.0
- rustls
- rust-mcp-sdk 0.8.3
- rust-mcp-transport 0.8.0

## Security

This implementation uses rustls for TLS support instead of OpenSSL, ensuring no external dependencies.