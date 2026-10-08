//! The winhands binary: `winhands daemon | mcp | status | acknowledge | pause | ...`.
//!
//! The subcommands land in later phases; this placeholder only links the core so the workspace
//! builds end to end.

fn main() {
    println!(
        "winhands {} (phase R1: core only, no subcommands yet)",
        env!("CARGO_PKG_VERSION")
    );
}
