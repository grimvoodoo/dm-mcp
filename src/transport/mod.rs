//! Transport-layer plumbing. The MCP handler is transport-agnostic; these modules attach it to
//! stdin/stdout or to a streamable-HTTP listener.

pub mod http;
pub mod stdio;
