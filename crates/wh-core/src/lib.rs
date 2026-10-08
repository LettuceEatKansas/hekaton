//! winhands core: everything above the platform line.
//!
//! This crate has no OS bindings and builds on Windows, macOS and Linux. It holds the configuration,
//! the safety layer (acknowledgement gate, destructive-action refusals, audit log, rate cap, human-input
//! pause), the key-combo parser, coordinate scaling, the image pipeline and the [`backend::Backend`]
//! trait that `wh-win` and `wh-mac` implement. The Python build (`winhands/`) is the specification;
//! each module's docs name the Python file it ports.
//!
//! No `unsafe` in this crate (`unsafe_code = "forbid"` in the workspace lints).

#![warn(missing_docs)]
