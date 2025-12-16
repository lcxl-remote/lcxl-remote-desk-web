use std::{sync::Arc, time::Instant};

use turn_server::{
    config::Config,
    statistics::Statistics,
    turn::{Observer, Service},
};

#[rustfmt::skip]
pub static SOFTWARE: &str = concat!(
    "lcxl-remote-desk-turn-rs.",
    env!("CARGO_PKG_VERSION")
);

/// TURN API state.
pub struct TurnApiState<T>
where
    T: Clone + Observer + 'static,
{
    pub config: Arc<Config>,
    pub service: Service<T>,
    pub statistics: Statistics,
    pub uptime: Instant,
}
