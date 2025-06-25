use std::sync::Once;

use log::LevelFilter;

static INIT: Once = Once::new();

pub fn initialize_logs() {
    INIT.call_once(|| {
        // initialization code here
        env_logger::builder()
            .format_timestamp_micros()
            .filter_level(LevelFilter::Debug)
            .init();

        //let result = ScreenRecordManager::set_thread_input_desktop();
        //log::info!("set thread desktop result: {:?}", result);
    });
}
