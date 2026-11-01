use log::LevelFilter;

use crate::error::DeskUtilsError;

pub fn init_logs(log_level: LevelFilter) -> Result<(), DeskUtilsError> {
    let result = env_logger::builder()
        .format_timestamp_micros()
        .filter_level(log_level)
        .try_init();
    if let Err(error) = result {
        log::warn!(
            "Failed to initialize logger, the logs may have already been initialized: {}",
            error
        );

        return Err(DeskUtilsError::from(error));
    }
    Ok(())
}
