use std::str::FromStr;

use log::LevelFilter;

use crate::desk_error::DeskError;

pub fn init_logs(log_level: LevelFilter) -> Result<(), DeskError> {
    let result = env_logger::builder()
        .format_timestamp_micros()
        .filter_level(log_level)
        .try_init();
    if let Err(error) = result {
        log::warn!(
            "Failed to initialize logger, the logs may have already been initialized: {}",
            error
        );

        return Err(DeskError::from(error));
    }
    Ok(())
}

pub fn init_logs_by_str(log_level: &str) -> Result<(), DeskError> {
    init_logs(LevelFilter::from_str(log_level)?)
}
