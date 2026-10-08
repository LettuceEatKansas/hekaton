//! Batching and verification (Python `computer.run` / `run_many`, `core.do_wait_for`, `_check_expect`).
//!
//! TODO(R1): `run_many` (steps, stop at the first error and name the step, one trailing screenshot
//! after [`crate::settle::settle`], `shot=false`, the "no image on a failure before any input" rule),
//! the `expect` checks (`text`, `gone_text`, `title`), `wait_for_text`, `wait_for_file`, the
//! `only_apps` allowlist (`_check_allowed`, `_check_launch_allowed`, `_check_irreversible`), and the
//! guard that refuses a window belonging to the caller. The building blocks it needs are in this
//! crate already: [`crate::scale`], [`crate::imaging`], [`crate::settle`], [`crate::human`],
//! [`crate::input`], [`crate::keys`], [`crate::destructive`] and the [`crate::backend::Backend`] trait.
