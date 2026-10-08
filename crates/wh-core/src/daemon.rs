//! The HTTP daemon (Python `winhands/daemon.py`).
//!
//! TODO(R1/R2): an `axum` server on loopback that speaks the Python daemon's API: `X-Winhands-Token`
//! auth ([`crate::token::verify`]), the acknowledgement gate ([`crate::ack::check_acknowledged`]),
//! per-call `allow_destructive` ([`crate::destructive::Policy::take_from_args`]), the rate cap and
//! pause flag ([`crate::state`]), the audit log ([`crate::audit`]), `/health`, batching with delta
//! reports, idle shutdown, OCR warm-up and the kill hotkey ([`crate::backend::Backend::register_kill_hotkey`]).
//! It will live in the `winhands` binary crate because it needs tokio and axum; this stub only marks
//! where the pure pieces it will call are.
