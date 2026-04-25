use std::sync::Once;

use log::LevelFilter;

static INIT: Once = Once::new();

pub fn initialize_logs() {
    INIT.call_once(|| {
        // initialization code here
        let _ = desk_utils::logs::init_logs(LevelFilter::Debug);

        //let result = ScreenRecordManager::set_thread_input_desktop();
        //log::info!("set thread desktop result: {:?}", result);
    });
}
