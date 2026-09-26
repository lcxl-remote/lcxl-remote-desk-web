//! Poll only while a locally accepted input period owns a frozen turn.
use crate::worker::agent::computer_use_broker::ComputerUseBroker;
use desk_ipc_protocol::message::WorkerToService;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::UnboundedSender;

pub(crate) struct TurnMonitor(tokio::task::JoinHandle<()>);
impl TurnMonitor {
    pub(crate) fn start(
        broker: Arc<ComputerUseBroker>,
        writer: UnboundedSender<WorkerToService>,
    ) -> Self {
        Self(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Some(query) = broker.linux_input_turn_probe()
                    && writer
                        .send(WorkerToService::ComputerTurnQuery(query))
                        .is_err()
                {
                    break;
                }
            }
        }))
    }
}
impl Drop for TurnMonitor {
    fn drop(&mut self) {
        self.0.abort();
    }
}
