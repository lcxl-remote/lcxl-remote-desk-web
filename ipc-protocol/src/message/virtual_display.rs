//! Virtual-display IPC payloads and state-machine outcomes.

use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

#[cfg(doc)]
use super::{ServiceToWorker, WorkerToService};
// ============= Virtual display IPC payloads =============

/// Payload for [`ServiceToWorker::SetVirtualDisplayMode`]. The browser
/// sends a `SignalingType::ChangeDisplaySettings`; the daemon validates
/// it (`desk_virtual_display::validate_mode`) and forwards it here. The
/// worker calls `VirtualDisplayController::set_mode` (driver pipe + CDS).
/// `request_id` correlates with [`WorkerToService::VirtualDisplayMode`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SetVirtualDisplayModePayload {
    pub request_id: String,
    pub connection_id: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Payload for [`ServiceToWorker::AttachVirtualDisplay`]. The daemon
/// holds the `SwDevice` handle and forwards the OS-assigned PnP
/// instance id (e.g. `SWD\LcxlVirtualDisplay\LcxlVirtualDisplay`) the
/// IDD monitor was assigned. The worker resolves the instance id to a
/// GDI `\\.\DISPLAYn` from inside the user session (where
/// `EnumDisplayDevicesW` actually sees the virtual monitor) and replies
/// with [`WorkerToService::VirtualDisplayAttachResult`]. The daemon
/// cannot resolve the display name itself because Session 0 (the
/// LocalSystem service desktop) does not see any GDI displays.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct AttachVirtualDisplayPayload {
    pub instance_id: String,
}

/// Wire form of `desk_virtual_display::VirtualDisplayMode`. Duplicated
/// here intentionally so `desk-ipc-protocol` does not need a reverse
/// dependency onto `desk-virtual-display`.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq, Hash,
)]
pub struct VirtualDisplayModeData {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Result of a worker-side `set_mode`. The IDD driver is free to snap
/// the requested mode to the nearest supported configuration, so
/// `Applied` carries what actually took effect, not what was requested.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(tag = "status", content = "data")]
pub enum VirtualDisplayModeOutcome {
    Applied(VirtualDisplayModeData),
    Failed(String),
}

/// Payload for [`WorkerToService::VirtualDisplayMode`]. Correlates with
/// the originating [`ServiceToWorker::SetVirtualDisplayMode`] via
/// `request_id` so the daemon's outbound classifier can wire it back to
/// the matching browser signaling websocket.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct VirtualDisplayModeResponsePayload {
    pub request_id: String,
    pub connection_id: String,
    pub outcome: VirtualDisplayModeOutcome,
}

/// Result of the worker resolving a PnP instance id (forwarded from the
/// daemon via [`ServiceToWorker::AttachVirtualDisplay`]) into a usable
/// GDI display name. Modelled as an explicit two-variant enum so the
/// wincode / serde wire shapes match the rest of `message.rs` rather
/// than the ad-hoc `Result<T, E>` envelope.
///
/// - `Attached(display_name)` — `display_name` is the GDI
///   `\\.\DISPLAYn` form the worker captured against.
/// - `Failed(message)` — exhaustive worker-side retries did not turn
///   up a GDI device matching the instance id (e.g. the driver
///   crashed, PnP node disappeared, or `EnumDisplayDevicesW` raced
///   with the IDD monitor arrival window).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
#[serde(tag = "status", content = "data")]
pub enum VirtualDisplayAttachOutcome {
    Attached(String),
    Failed(String),
}

/// Payload for [`WorkerToService::VirtualDisplayAttachResult`]. The
/// `instance_id` field correlates the reply with a specific
/// `SwDeviceCreate` round so the supervisor can drop stale replies
/// that arrive after the daemon has re-created the underlying handle
/// (i.e. after a daemon restart, where the PnP id is identical but the
/// in-memory supervisor state is fresh).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
pub struct VirtualDisplayAttachResultPayload {
    pub instance_id: String,
    pub outcome: VirtualDisplayAttachOutcome,
}

/// Payload for [`ServiceToWorker::SetVirtualDisplayExclusive`].
///
/// `op_id` is monotonically incremented by the daemon's supervisor
/// each time it issues a new exclusive command (enter or leave). The
/// worker stores it on the runner currently doing the work and feeds
/// it back via [`ExclusiveResultPayload::op_id`] so the daemon can
/// drop stale results from a superseded runner.
///
/// `prompt_duration_ms` is the system-level
/// `Settings.virtual_display.prompt_ms` snapshot at the moment the
/// daemon decided to enter exclusive. `0` skips the prompt entirely.
/// Ignored on a `desired = false` request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
pub struct SetVirtualDisplayExclusivePayload {
    pub op_id: u64,
    pub desired: bool,
    pub prompt_duration_ms: u32,
}

/// Direction the worker was driving when it produced this result.
/// Disambiguates [`ExclusiveOutcome::Entered`] vs `Left` at the
/// daemon state machine, which transitions different states for each.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExclusiveDirection {
    Entering,
    Leaving,
}

/// Outcome reported by the worker's exclusive runner. Four variants
/// only — `EnterCancelled` was removed in design review round 6
/// because the new pipeline never emits one: a cancelled enter
/// returns silently and the next runner publishes the actual final
/// state (Entered / Left).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
#[serde(tag = "status", content = "data")]
pub enum ExclusiveOutcome {
    Entered,
    EnterFailed(String),
    Left,
    LeftWithErrors(String),
}

/// Payload for [`WorkerToService::ExclusiveResult`]. `op_id` echoes
/// the originating request; the daemon's supervisor drops anything
/// whose `op_id != current_op_id` at the lock boundary.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
pub struct ExclusiveResultPayload {
    pub op_id: u64,
    pub direction: ExclusiveDirection,
    pub outcome: ExclusiveOutcome,
}
