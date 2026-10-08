//! The MCP `computer` tool (Python `winhands/shim.py`).
//!
//! TODO(R1): the exact tool schema (`shot`, `shot_long` default 1024, `wake`, slimmed schema, the
//! `allow_destructive` opt-in), served by `winhands mcp` over stdio with `rmcp` in the `winhands`
//! binary crate. The action vocabulary it validates against is [`crate::actions`]. Tests to port:
//! the schema tests in `test_20261008_speed_fixes.py` (W6, W7: smaller definition, every old
//! parameter kept, no em dashes) and `test_shim_results.py`.
