//! # desk-ipc-protocol
//!
//! IPC protocol definitions for Service ↔ Worker and Service ↔ UI communication.
//!
//! ## Architecture
//!
//! ```text
//! Service Core (SYSTEM)  <--IPC-->  Desk Worker (User Session)
//!       ^                                  ^
//!       |                                  |
//!   ServiceToWorker                   WorkerToService
//!   WorkerToService                   ServiceToWorker
//! ```
//!
//! Communication uses named pipes (Windows) or Unix domain sockets (Linux/macOS).
//! Wire format: length-prefixed bincode v2 (`bincode::config::standard()`).
//! See [`transport`] for the framing details.

pub mod dual_transport;
pub mod message;
pub mod transport;
