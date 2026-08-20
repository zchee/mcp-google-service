//! Unified Google Cloud MCP aggregator.
//!
//! Aggregates Google's per-service `https://{service}.googleapis.com/mcp`
//! endpoints behind one auth model, one namespaced tool surface, and one
//! error classifier. The `mcp-google-service` binary is a thin stdio front end
//! over these modules; they are exposed as a library so the integration tier
//! can assemble a server against in-process upstreams.

pub mod auth;
pub mod catalog;
pub mod config;
pub mod error;
pub mod proxy;
pub mod prune;
pub mod registry;
pub mod server;
