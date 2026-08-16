use super::*;

pub fn from_rect(rect: &RECT) -> DisplayRect {
    DisplayRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

pub fn from_dxgi_output_desc(output_desc: &DXGI_OUTPUT_DESC) -> DisplayInfo {
    log::debug!(
        "Converting DXGI_OUTPUT_DESC to DisplayInfo, output_desc: {:?}",
        output_desc
    );

    let null_char_index = output_desc
        .DeviceName
        .iter()
        .position(|&item| item == 0u16)
        .unwrap_or(output_desc.DeviceName.len());
    let device_name: String = String::from_utf16_lossy(&output_desc.DeviceName[..null_char_index]);

    let mut display_device = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        DeviceName: [0u16; 32],
        DeviceString: [0u16; 128],
        StateFlags: DISPLAY_DEVICE_STATE_FLAGS(0),
        DeviceID: [0u16; 128],
        DeviceKey: [0u16; 128],
    };
    let succeed = unsafe {
        EnumDisplayDevicesW(
            PCWSTR::from_raw(output_desc.DeviceName.as_ptr()),
            0,
            &mut display_device,
            0,
        )
    };
    let display_device_name = if succeed.as_bool() {
        // DEBUG, not INFO: this is on the per-frame `get_current_output`
        // path for any caller that re-queries display info each tick;
        // emitting at INFO floods the log. Static metadata on the
        // adapter is logged once at capture construction in
        // `ScreenRecordManager`, which is the appropriate place for
        // operator-visible enumeration output.
        log::debug!(
            "Successfully enumerated display device: {:?}",
            display_device
        );
        let null_char_index = display_device
            .DeviceString
            .iter()
            .position(|&item| item == 0u16)
            .unwrap_or(output_desc.DeviceName.len());
        let name: String =
            String::from_utf16_lossy(&display_device.DeviceString[..null_char_index]);

        log::debug!("Display device name: {}", name);
        Some(name)
    } else {
        None
    };
    let desktop_coordinates = from_rect(&output_desc.DesktopCoordinates);
    let attached_to_desktop = output_desc.AttachedToDesktop.as_bool();
    let rotation = output_desc.Rotation.0;

    // DEBUG, not INFO: see rationale on the `Successfully enumerated
    // display device` log just above. Per-frame callers would otherwise
    // emit this at the OS refresh rate.
    log::debug!(
        "Found output, name={}, display_device_name={:?}, desktop_coordinates={:?}, attached_to_desktop={}, rotation={}",
        device_name,
        display_device_name,
        desktop_coordinates,
        attached_to_desktop,
        rotation
    );

    let resolutions = enum_display_resolutions(&device_name).unwrap_or_default();

    DisplayInfo {
        device_name,
        display_device_name,
        desktop_coordinates,
        resolutions,
        attached_to_desktop,
        rotation,
        current_capture_resolution: None,
    }
}

// ============================================================================
// Cross-adapter output enumeration
// ============================================================================

/// One DXGI output joined with the adapter that owns it. The flat
/// ordering of a `Vec<EnumeratedOutput>` is the order presented to the
/// frontend dropdown: the default hardware adapter is placed first
/// (so existing single-GPU users see no behavior change), then the
/// remaining adapters follow in `IDXGIFactory1::EnumAdapters1` order.
/// Selection is by `DeskSettings::video_device_name` against the GDI
/// device name embedded in each entry's `DXGI_OUTPUT_DESC.DeviceName`.
#[derive(Clone)]
pub(crate) struct EnumeratedOutput {
    pub adapter_index: u32,
    pub local_output_index: u32,
    pub adapter: IDXGIAdapter1,
    pub desc: DXGI_OUTPUT_DESC,
    pub adapter_desc: DXGI_ADAPTER_DESC1,
}

/// Extract the GDI device name (`\\.\DISPLAYn`) from a DXGI output
/// descriptor. The 32-wchar field is null-terminated.
pub(crate) fn output_device_name(desc: &DXGI_OUTPUT_DESC) -> String {
    let nul = desc
        .DeviceName
        .iter()
        .position(|&c| c == 0u16)
        .unwrap_or(desc.DeviceName.len());
    String::from_utf16_lossy(&desc.DeviceName[..nul])
}

/// Pure device-name selection: given a flat slice of GDI device names
/// in flat-enumeration order, return the position whose entry exactly
/// matches `requested`. Pure (no DXGI fixtures) so the
/// not-found / empty-string branches are exhaustively testable.
///
/// The not-found error lists every enumerated name so hot-plug
/// reordering or detached-monitor cases are obvious at a glance. WGC
/// is mentioned as an alternative backend only for triage: both
/// backends now enumerate the same set of GDI device names (DXGI via
/// `IDXGIAdapter::EnumOutputs`, WGC via `EnumDisplayMonitors`,
/// including IDD virtual displays attached through `LcxlVirtualDisplay`),
/// so a name missing on one is usually missing on the other too.
pub(crate) fn find_device_name_index(
    names: &[String],
    requested: &str,
) -> Result<usize, CaptureError> {
    if requested.is_empty() {
        return CaptureError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "video_device_name is empty: no display has been selected. \
             Open the desktop dialog in the browser and pick a display \
             before starting media.",
        );
    }
    if let Some(idx) = names.iter().position(|n| n == requested) {
        return Ok(idx);
    }
    let summary = if names.is_empty() {
        "(none)".to_string()
    } else {
        names
            .iter()
            .map(|n| format!("{:?}", n))
            .collect::<Vec<_>>()
            .join(", ")
    };
    CaptureError::custom_error(
        DeskErrorCode::INVALID_PARAMS,
        &format!(
            "device_name {:?} not enumerated by DXGI; enumerated: [{}]. \
             The display may be detached, asleep, or re-ordered by a \
             hot-plug event; re-open the desktop dialog to pick a \
             currently attached display. If the device is listed by the \
             WGC backend but not here, switching to WGC is a viable \
             workaround.",
            requested, summary
        ),
    )
}

/// Pure ordering: returns the permutation (indices into `adapter_luids`)
/// that places `default_luid` first when present, and keeps the rest in
/// their original factory order. If `default_luid` is `None` or its LUID
/// is not found in `adapter_luids`, the identity permutation is
/// returned. Extracted so it is unit-testable without DXGI fixtures.
pub(crate) fn order_adapters_by_default_luid(
    default_luid: Option<LUID>,
    adapter_luids: &[LUID],
) -> Vec<usize> {
    let identity: Vec<usize> = (0..adapter_luids.len()).collect();
    let Some(target) = default_luid else {
        return identity;
    };
    let Some(promoted) = adapter_luids
        .iter()
        .position(|l| l.LowPart == target.LowPart && l.HighPart == target.HighPart)
    else {
        return identity;
    };
    let mut order = Vec::with_capacity(adapter_luids.len());
    order.push(promoted);
    for (i, _) in adapter_luids.iter().enumerate() {
        if i != promoted {
            order.push(i);
        }
    }
    order
}

/// Capture the LUID of the adapter that `D3D11CreateDevice(None, HARDWARE)`
/// would pick, so cross-adapter enumeration can promote it to flat
/// position 0 and keep single-default-adapter users seeing the same
/// dropdown order. Returns `None` (without erroring) if the probe
/// fails — callers fall back to factory order.
fn probe_default_adapter_luid() -> Option<LUID> {
    let driver_types: [D3D_DRIVER_TYPE; 3] = [
        D3D_DRIVER_TYPE_HARDWARE,
        D3D_DRIVER_TYPE_WARP,
        D3D_DRIVER_TYPE_REFERENCE,
    ];
    let feature_levels: [D3D_FEATURE_LEVEL; 4] = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_9_1,
    ];
    let mut device: Option<ID3D11Device> = None;
    for driver_type in driver_types {
        let r = unsafe {
            D3D11CreateDevice(
                None,
                driver_type,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        if r.is_ok() {
            break;
        }
    }
    let device = device?;
    let dxgi_device = device.cast::<IDXGIDevice>().ok()?;
    let adapter = unsafe { dxgi_device.GetParent::<IDXGIAdapter>().ok()? };
    let adapter1 = adapter.cast::<IDXGIAdapter1>().ok()?;
    let desc = unsafe { adapter1.GetDesc1().ok()? };
    Some(desc.AdapterLuid)
}

/// Enumerate every output across every DXGI adapter. See
/// [`EnumeratedOutput`] for the flat ordering contract.
pub(crate) fn enumerate_all_outputs() -> Result<Vec<EnumeratedOutput>, CaptureError> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;

    // Phase 1: collect every adapter (preserve factory order).
    struct AdapterEntry {
        adapter: IDXGIAdapter1,
        desc: DXGI_ADAPTER_DESC1,
        luid: LUID,
        output_descs: Vec<DXGI_OUTPUT_DESC>,
    }
    let mut adapters: Vec<AdapterEntry> = Vec::new();
    let mut adapter_idx: u32 = 0;
    loop {
        let r = unsafe { factory.EnumAdapters1(adapter_idx) };
        let adapter = match r {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(CaptureError::from(e)),
        };
        let desc = unsafe { adapter.GetDesc1() }?;
        let luid = desc.AdapterLuid;
        let mut output_descs: Vec<DXGI_OUTPUT_DESC> = Vec::new();
        let mut out_idx: u32 = 0;
        loop {
            let r = unsafe { adapter.EnumOutputs(out_idx) };
            let output = match r {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(CaptureError::from(e)),
            };
            let output_desc = unsafe { output.GetDesc() }?;
            output_descs.push(output_desc);
            out_idx += 1;
        }
        adapters.push(AdapterEntry {
            adapter,
            desc,
            luid,
            output_descs,
        });
        adapter_idx += 1;
    }

    // Phase 2: reorder adapters so the default hardware adapter is
    // first. Preserves dropdown ordering for users who only ever saw
    // default-adapter outputs.
    let default_luid = probe_default_adapter_luid();
    let luids: Vec<LUID> = adapters.iter().map(|e| e.luid).collect();
    let order = order_adapters_by_default_luid(default_luid, &luids);

    // Phase 3: flatten according to the new order.
    let mut flat: Vec<EnumeratedOutput> = Vec::new();
    for (new_adapter_idx, &orig_idx) in order.iter().enumerate() {
        let entry = &adapters[orig_idx];
        for (local_idx, output_desc) in entry.output_descs.iter().enumerate() {
            flat.push(EnumeratedOutput {
                adapter_index: new_adapter_idx as u32,
                local_output_index: local_idx as u32,
                adapter: entry.adapter.clone(),
                desc: *output_desc,
                adapter_desc: entry.desc,
            });
        }
    }
    log::info!(
        "enumerate_all_outputs: {} adapter(s), {} output(s) total, default_luid={}",
        adapters.len(),
        flat.len(),
        match default_luid {
            Some(l) => format!("Some({:?})", (l.LowPart, l.HighPart)),
            None => "None".to_string(),
        }
    );
    Ok(flat)
}

/// Resolve a GDI device name against the flat enumeration. On
/// failure, the error message includes the requested name, every
/// enumerated device_name, a per-adapter summary (for multi-GPU
/// triage), and the hot-plug / WGC-alternative hint generated by
/// [`find_device_name_index`].
pub(crate) fn select_output_by_name<'a>(
    entries: &'a [EnumeratedOutput],
    device_name: &str,
) -> Result<&'a EnumeratedOutput, CaptureError> {
    let names: Vec<String> = entries
        .iter()
        .map(|e| output_device_name(&e.desc))
        .collect();
    match find_device_name_index(&names, device_name) {
        Ok(idx) => Ok(&entries[idx]),
        Err(e) => {
            // Re-wrap to append the per-adapter summary, which the pure
            // helper cannot compute (it does not see EnumeratedOutput).
            CaptureError::custom_error(
                DeskErrorCode::INVALID_PARAMS,
                &format!(
                    "{} adapter_summary: ({})",
                    e,
                    build_adapter_summary(entries)
                ),
            )
        }
    }
}

/// Render a `"adapter[i]='Name' K output(s); ..."` string describing
/// the flat enumeration. Helper for `select_output` error messages.
fn build_adapter_summary(entries: &[EnumeratedOutput]) -> String {
    let mut summary = String::new();
    let mut current_adapter: i64 = -1;
    let mut current_count: u32 = 0;
    let mut current_name = String::new();
    for e in entries {
        if e.adapter_index as i64 != current_adapter {
            if current_adapter >= 0 {
                summary.push_str(&format!(
                    "adapter[{}]='{}' {} output(s); ",
                    current_adapter, current_name, current_count
                ));
            }
            current_adapter = e.adapter_index as i64;
            current_count = 0;
            current_name = adapter_name_from_desc(&e.adapter_desc);
        }
        current_count += 1;
    }
    if current_adapter >= 0 {
        summary.push_str(&format!(
            "adapter[{}]='{}' {} output(s)",
            current_adapter, current_name, current_count
        ));
    }
    summary
}

/// Convert `DXGI_ADAPTER_DESC1::Description` ([u16; 128]) to a String,
/// stopping at the first NUL.
pub(crate) fn adapter_name_from_desc(desc: &DXGI_ADAPTER_DESC1) -> String {
    let null = desc
        .Description
        .iter()
        .position(|&c| c == 0u16)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..null])
}
