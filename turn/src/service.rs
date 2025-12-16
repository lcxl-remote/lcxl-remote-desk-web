use std::{sync::Arc, time::Instant};

use turn_server::{
    config::Config,
    statistics::Statistics,
    turn::{Observer, Service},
};

use crate::model::{SOFTWARE, TurnApiState};

/// Starts the TURN server with the provided config and observer.
pub async fn startup_turn_server<T>(
    config: Arc<Config>,
    observer: T,
) -> anyhow::Result<TurnApiState<T>>
where
    T: Clone + Observer + 'static,
{
    log::info!("Starting turn server with config {:?}", config);

    let statistics = Statistics::default();
    let service = Service::new(
        SOFTWARE.to_string(),
        config.turn.realm.clone(),
        config.turn.get_externals(),
        observer,
    );

    turn_server::server::start(&config, &statistics, &service).await?;
    let api_state = TurnApiState {
        config: config.clone(),
        uptime: Instant::now(),
        service,
        statistics,
    };

    log::info!("Turn server starteds successfully.");
    Ok(api_state)
}
