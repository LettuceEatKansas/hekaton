//! The Windows backend: UI Automation, SendInput, GDI/DXGI capture, WinRT OCR.
//!
//! On every other OS this crate is empty. The implementation lands in phase R2; it will implement
//! `wh_core::backend::Backend`.
#![cfg(windows)]
