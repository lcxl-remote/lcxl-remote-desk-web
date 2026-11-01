//! Shared watchdogs for host integration tests; production deadlines are independent.

use std::time::Duration;

pub const MODEL_IO_TIMEOUT: Duration = Duration::from_secs(30);
pub const SCENARIO_TIMEOUT: Duration = Duration::from_secs(120);
