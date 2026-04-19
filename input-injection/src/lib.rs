//! # desk-input-injection
//!
//! Input injection and host control engine for lcxl-remote-desk.
//!
//! This crate provides platform-abstracted interfaces and implementations for:
//! - Mouse event injection (Windows, Linux/uinput, Linux/Wayland Portal, macOS)
//! - Keyboard event injection (Windows, Linux/uinput, Linux/Wayland Portal, macOS)
//! - Host control (display settings, block input, private screen, clipboard)
//!
//! It is a pure library crate with no dependency on web frameworks (actix-web)
//! or WebRTC. Both `web/server` and `desk-worker` depend on this crate.

pub mod error;
pub mod model;

pub mod mouse_event;
pub mod keyboard_event;
pub mod host_control;
