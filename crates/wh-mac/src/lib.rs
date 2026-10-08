//! The macOS backend: CGEvent input, ScreenCaptureKit capture, the AX tree, Vision OCR.
//!
//! On every other OS this crate is empty. The implementation lands in phase R5; it will implement
//! `wh_core::backend::Backend`.
#![cfg(target_os = "macos")]
