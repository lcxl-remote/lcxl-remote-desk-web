//! # desk-capture-engine
//!
//! Screen/audio capture and video/audio encoding engine for lcxl-remote-desk.
//!
//! This crate provides platform-abstracted interfaces and implementations for:
//! - Screen capture (DXGI, GDI, X11, Wayland Portal, ScreenCaptureKit)
//! - Video encoding (H264, VP8/9, X264, AV1)
//! - Audio capture (WASAPI, PipeWire, ScreenCaptureKit)
//! - Audio encoding (Opus)
//!
//! It is a pure library crate with no dependency on web frameworks (actix-web)
//! or WebRTC. Both `web/server` and `desk-worker` depend on this crate.

pub mod error;
pub mod model;

pub mod image_capture;
pub mod video_encoder;
pub mod audio_capture;
pub mod audio_encoder;
