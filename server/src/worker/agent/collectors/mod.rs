//! Read-context collectors.
//!
//! Each collector turns one [`desk_agent_protocol::ContextKind`] into its
//! structured output. Collectors are synchronous (they wrap blocking system
//! probes) and are invoked from the async dispatch via `spawn_blocking`, so
//! they must never assume an async runtime is available.
//!
//! Cross-platform probes (sysinfo) need no per-OS branch and live in a single
//! file; OS-specific probes (network ports, service status, event log) split
//! their implementation behind a platform trait as they land.

pub mod container;
pub mod log_recent;
pub mod network_ports;
pub mod process_list;
pub mod service_status;
pub mod system_info;
