//! The macOS backend: CGEvent input, ScreenCaptureKit capture, the AX tree, Vision OCR.
//!
//! The pure helpers below (key table, point and pixel arithmetic, AX role mapping, OCR box
//! flipping, window-list parsing, typing plans) build and are unit-tested on every OS. The
//! platform layer that implements `wh_core::backend::Backend` follows in the next commit.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
#![allow(dead_code)]

mod geom;
mod keymap;
mod launchutil;
mod ocrgeom;
mod roles;
mod textops;
mod wininfo;
