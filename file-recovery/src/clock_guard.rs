//! Compare wall time with an OS clock that includes sleep and cannot be set.
use serde::{Deserialize, Serialize};
use std::io;

const MAX_DRIFT_MS: u64 = 5 * 60 * 1000;

#[derive(Clone, Serialize, Deserialize)]
struct Observation {
    boot_id: String,
    wall_ms: u64,
    elapsed_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Guard {
    anchor: Option<Observation>,
    pub blocked: bool,
}

impl Guard {
    pub fn initialized(&self) -> bool {
        self.anchor.is_some()
    }
    pub fn validate(&self) -> io::Result<()> {
        if self
            .anchor
            .as_ref()
            .is_some_and(|sample| !super::valid_id(&sample.boot_id) || sample.wall_ms == 0)
        {
            return Err(super::invalid("invalid recovery clock anchor"));
        }
        Ok(())
    }

    fn observe(&mut self, next: Observation) {
        if let Some(previous) = &self.anchor {
            if previous.boot_id == next.boot_id {
                self.blocked = next
                    .elapsed_ms
                    .checked_sub(previous.elapsed_ms)
                    .and_then(|elapsed| previous.wall_ms.checked_add(elapsed))
                    .is_none_or(|expected| expected.abs_diff(next.wall_ms) > MAX_DRIFT_MS);
                // Keep the original anchor: repeated small jumps must not reset
                // the comparison, and a worker restart must not clear a pause.
                return;
            }
            if self.blocked || next.wall_ms < previous.wall_ms {
                self.blocked = true;
                return;
            }
        }
        // Offline time cannot be measured by the previous boot's elapsed clock.
        // A normal new boot starts a new anchor; a known anomaly stays blocked.
        self.anchor = Some(next);
        self.blocked = false;
    }

    pub fn observe_system(&mut self) {
        match system_observation() {
            Ok(sample) => self.observe(sample),
            Err(_) => self.blocked = true,
        }
    }
    fn acknowledge(&mut self, sample: Observation, displayed_wall_ms: u64) -> io::Result<u64> {
        if displayed_wall_ms == 0 || sample.wall_ms.abs_diff(displayed_wall_ms) > 60_000 {
            return Err(super::invalid(
                "device time changed since confirmation; refresh and confirm again",
            ));
        }
        let wall = sample.wall_ms;
        self.anchor = Some(sample);
        self.blocked = false;
        Ok(wall)
    }
    pub fn acknowledge_system(&mut self, displayed_wall_ms: u64) -> io::Result<u64> {
        self.acknowledge(system_observation()?, displayed_wall_ms)
    }
}

fn system_observation() -> io::Result<Observation> {
    let (boot_id, elapsed_ms) = system_boot_clock()?;
    let wall_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
    )
    .map_err(io::Error::other)?;
    if wall_ms == 0 {
        return Err(super::invalid("invalid system time"));
    }
    Ok(Observation {
        boot_id: super::hash(boot_id.as_bytes()),
        wall_ms,
        elapsed_ms,
    })
}

#[cfg(target_os = "macos")]
fn system_boot_clock() -> io::Result<(String, u64)> {
    use mach2::mach_time::{mach_continuous_time, mach_timebase_info};
    let mut bytes = [0_u8; 128];
    let mut size = bytes.len();
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let boot = std::str::from_utf8(
        bytes
            .get(..size)
            .ok_or_else(|| super::invalid("invalid boot identity size"))?,
    )
    .map_err(io::Error::other)?
    .trim_end_matches('\0')
    .to_owned();
    if boot.is_empty() {
        return Err(super::invalid("missing boot identity"));
    }
    let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
    if unsafe { mach_timebase_info(&mut info) } != 0 || info.denom == 0 {
        return Err(io::Error::other("continuous clock unavailable"));
    }
    let nanos = u128::from(unsafe { mach_continuous_time() }) * u128::from(info.numer)
        / u128::from(info.denom);
    Ok((
        boot,
        u64::try_from(nanos / 1_000_000).map_err(io::Error::other)?,
    ))
}

#[cfg(target_os = "linux")]
fn system_boot_clock() -> io::Result<(String, u64)> {
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    if boot.trim().is_empty() || boot.len() > 128 {
        return Err(super::invalid("invalid boot identity"));
    }
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let elapsed = u64::try_from(value.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|ms| {
            u64::try_from(value.tv_nsec)
                .ok()
                .and_then(|ns| ms.checked_add(ns / 1_000_000))
        })
        .ok_or_else(|| super::invalid("invalid boot clock"))?;
    Ok((boot.trim().into(), elapsed))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn system_boot_clock() -> io::Result<(String, u64)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "recovery clock unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(wall_ms: u64, elapsed_ms: u64) -> Observation {
        Observation {
            boot_id: "a".repeat(64),
            wall_ms,
            elapsed_ms,
        }
    }
    #[test]
    fn confirmation_rejects_stale_displayed_time_and_reanchors_a_blocked_boot() {
        let mut guard = Guard::default();
        guard.observe(sample(1_000_000, 100));
        guard.observe(sample(9_000_000, 200));
        assert!(
            guard
                .acknowledge(sample(9_000_000, 300), 1_000_000)
                .is_err()
        );
        assert!(guard.blocked);
        assert_eq!(
            guard
                .acknowledge(sample(9_000_000, 300), 9_000_000)
                .unwrap(),
            9_000_000
        );
        guard.observe(sample(9_000_100, 400));
        assert!(!guard.blocked);
        guard.observe(sample(19_000_000, 500));
        assert!(guard.blocked);
    }
    #[test]
    #[ignore = "requires access to the host boot identity and continuous clock"]
    fn real_system_clock_has_a_stable_boot_identity_and_monotonic_elapsed_time() {
        let first = system_observation().unwrap();
        let next = system_observation().unwrap();
        assert_eq!(first.boot_id, next.boot_id);
        assert!(super::super::valid_id(&first.boot_id));
        assert!(next.elapsed_ms >= first.elapsed_ms);
        assert!(next.wall_ms.abs_diff(first.wall_ms) < 1000);
    }
    #[test]
    fn forward_jump_stays_blocked_after_worker_restart_and_recovers_when_corrected() {
        let mut guard = Guard::default();
        guard.observe(sample(1_000_000, 100));
        guard.observe(sample(9_000_000, 200));
        assert!(guard.blocked);
        let mut reopened: Guard =
            serde_json::from_slice(&serde_json::to_vec(&guard).unwrap()).unwrap();
        reopened.observe(sample(9_000_100, 300));
        assert!(reopened.blocked);
        reopened.observe(sample(1_000_300, 400));
        assert!(!reopened.blocked);
    }
    #[test]
    fn sleep_and_offline_time_are_distinct_from_observed_clock_jumps() {
        let mut guard = Guard::default();
        guard.observe(sample(1_000_000, 100));
        guard.observe(sample(9_000_000, 8_000_100));
        assert!(!guard.blocked);
        let mut reboot = sample(90_000_000, 100);
        reboot.boot_id = "b".repeat(64);
        guard.observe(reboot.clone());
        assert!(!guard.blocked);
        guard.observe(Observation {
            wall_ms: 190_000_000,
            ..reboot.clone()
        });
        assert!(guard.blocked);
        reboot.boot_id = "c".repeat(64);
        guard.observe(reboot);
        assert!(
            guard.blocked,
            "reboot cannot acknowledge a known clock anomaly"
        );
    }
    #[test]
    fn repeated_small_jumps_do_not_move_the_anchor() {
        let mut guard = Guard::default();
        guard.observe(sample(1_000_000, 100));
        guard.observe(sample(1_200_100, 200));
        assert!(!guard.blocked);
        guard.observe(sample(1_400_200, 300));
        assert!(guard.blocked);
    }
}
