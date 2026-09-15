//! A boot-scoped, sleep-inclusive clock for persistent recovery decisions.
use std::{io, mem::size_of};
use windows::{
    Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS},
    Win32::System::SystemInformation::GetTickCount64,
};

// Windows 8+ SYSTEM_BOOT_ENVIRONMENT_INFORMATION layout. This information class
// is not named in windows-rs 0.61.3; keep its ABI isolated and reject unknown
// layouts rather than estimating boot identity from mutable wall time.
// Reference: winsiderss/phnt, ntexapi.h, SystemBootEnvironmentInformation.
const BOOT_ENVIRONMENT: SYSTEM_INFORMATION_CLASS = SYSTEM_INFORMATION_CLASS(90);
#[repr(C, align(8))]
#[derive(Default)]
struct BootEnvironment {
    boot_identifier: [u8; 16],
    firmware_type: u32,
    padding: u32,
    boot_flags: u64,
}

pub(super) fn system_boot_clock() -> io::Result<(String, u64)> {
    let mut info = BootEnvironment::default();
    let mut returned = 0;
    let status = unsafe {
        NtQuerySystemInformation(
            BOOT_ENVIRONMENT,
            (&mut info as *mut BootEnvironment).cast(),
            size_of::<BootEnvironment>() as u32,
            &mut returned,
        )
    };
    if status.0 < 0
        || returned != size_of::<BootEnvironment>() as u32
        || info.boot_identifier.iter().all(|byte| *byte == 0)
    {
        return Err(io::Error::other(
            "Windows recovery boot identity unavailable",
        ));
    }
    // GetTickCount64 includes sleep/hibernation and is not changed by setting
    // the system wall clock. The caller separately samples wall time and hashes
    // the boot identifier before storing it in the private ledger.
    let elapsed_ms = unsafe { GetTickCount64() };
    let boot_id = info
        .boot_identifier
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((boot_id, elapsed_ms))
}
