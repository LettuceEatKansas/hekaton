//! winhands core: everything above the platform line.
//!
//! This crate has no OS bindings and builds on Windows, macOS and Linux. It holds the configuration,
//! the safety layer (acknowledgement gate, destructive-action refusals, audit log, rate cap, human-input
//! pause), the key-combo parser, coordinate scaling, the image pipeline, the caller guard, the
//! `computer` tool itself ([`orchestrate`]: batching, every action, the allowlist) and the [`backend::Backend`]
//! trait that `wh-win` and `wh-mac` implement. The Python build (`winhands/`) is the specification;
//! each module's docs name the Python file it ports.
//!
//! No `unsafe` in this crate (`unsafe_code = "forbid"` in the workspace lints).

#![warn(missing_docs)]

pub mod ack;
pub mod actions;
pub mod audit;
pub mod backend;
pub mod caller;
pub mod clock;
pub mod config;
pub mod destructive;
pub mod env;
pub mod folders;
pub mod human;
pub mod imaging;
pub mod input;
pub mod keys;
pub mod paths;
pub mod prof;
pub mod recipes;
pub mod redact;
pub mod request;
pub mod safety;
pub mod scale;
pub mod settle;
pub mod state;
pub mod textutil;
pub mod token;
pub mod tool_schema;

pub mod orchestrate;

// Later phases. Each stub says what it will port.
pub mod daemon;
pub mod mcp;

#[cfg(test)]
pub(crate) mod testutil;
