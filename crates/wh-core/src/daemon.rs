//! The HTTP daemon (Python `winhands/daemon.py`).
//!
//! TODO(R1/R2): an `axum` server on loopback that speaks the Python daemon's API: `X-Winhands-Token`
//! auth ([`crate::token::verify`]), the acknowledgement gate ([`crate::ack::check_acknowledged`]),
//! per-call `allow_destructive` ([`crate::destructive::Policy::take_from_args`]), the rate cap and
//! pause flag ([`crate::state`]), the audit log ([`crate::audit`]), `/health`, batching with delta
//! reports, idle shutdown, OCR warm-up and the kill hotkey ([`crate::backend::Backend::register_kill_hotkey`]).
//! It will live in the `winhands` binary crate because it needs tokio and axum; this stub only marks
//! where the pure pieces it will call are.
//!
//! Since R1b the `computer` route is one call: [`crate::orchestrate::Computer::execute`] does the
//! pause flag, the acknowledgement gate, the rate cap, the per-call destructive opt-in, one action at
//! a time, the audit entry and the `failed_step` fields, and returns the JSON body
//! ([`crate::orchestrate::ToolOutcome::to_json`]). The read routes call
//! [`Computer::windows_text`](crate::orchestrate::Computer::windows_text),
//! [`Computer::read_snapshot`](crate::orchestrate::Computer::read_snapshot) and
//! [`Computer::list_files`](crate::orchestrate::Computer::list_files), each answer passed through
//! [`Computer::repeat_note`](crate::orchestrate::Computer::repeat_note). What stays here: the HTTP
//! server, the token check, `/health`, `/shutdown`, the per-connection
//! [`caller::identify`](crate::caller::identify), the OCR warm-up gate (`_wait_for_warm`), the idle
//! thread and the batch route that wraps several tools.
