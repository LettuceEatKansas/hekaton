//! The `winhands` binary's code: the HTTP daemon, the MCP stdio server and the command line.
//!
//! The logic that needs no sockets and no OS lives in `wh-core` (`wh_core::daemon`, `wh_core::mcp`,
//! the `computer` tool itself). This crate adds what does: the HTTP server round the daemon, the HTTP
//! client the shim and `wh` use, the rmcp server, process start-up and the platform backend.

pub mod cli;
pub mod client;
pub mod link;
pub mod platform;
pub mod registry;
pub mod serve;
pub mod shim;
pub mod start;
pub mod wh;
