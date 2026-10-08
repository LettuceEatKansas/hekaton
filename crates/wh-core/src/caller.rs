//! Who is driving winhands right now (Python `winhands/caller.py` plus `core.agent_host_refusal`).
//!
//! TODO(R1 follow-up): port `Caller` (chain, role, describe, app_names), `GENERIC_HOSTS`,
//! `PROXY_HOSTS`, `identify` and `process_chain` on top of
//! [`Backend::peer_pid`](crate::backend::Backend::peer_pid),
//! [`Backend::process_table`](crate::backend::Backend::process_table) and
//! [`Backend::process_info`](crate::backend::Backend::process_info) (the
//! parent-created-after-child recycled-pid rule uses
//! [`ProcessInfo::started_at`](crate::backend::ProcessInfo::started_at)), then `agent_host_refusal`
//! with the `protect_processes` list and the `AGENT_HOST_PROCESSES` fallback. Tests to port:
//! `test_20260924_caller_guard.py`, `test_20260924_remote_caller.py`, `test_caller_scope.py`.
//!
//! The Backend surface for it already exists; nothing here is blocked.
