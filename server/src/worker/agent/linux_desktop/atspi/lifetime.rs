//! A subscription epoch fences object paths across tree replacement and bus loss.
use super::super::monitor_owner::MonitorLease;
use super::*;
use std::sync::{Mutex, Weak};
mod events;
mod subscription;

static CURRENT: Mutex<(Option<String>, u64)> = Mutex::new((None, 0));

fn publish(bus: Option<String>) {
    let mut current = CURRENT.lock().expect("AT-SPI epoch lock");
    current.0 = bus;
    current.1 = current.1.checked_add(1).expect("AT-SPI epoch exhausted");
}
pub(crate) fn ready() -> bool {
    CURRENT.lock().is_ok_and(|state| state.0.is_some())
}
pub(super) fn epoch(bus: &str) -> Result<u64> {
    let current = CURRENT
        .lock()
        .map_err(|_| error("AT-SPI epoch unavailable"))?;
    if current.0.as_deref() != Some(bus) {
        return Err(error("AT-SPI lifecycle monitor unavailable"));
    }
    Ok(current.1)
}

pub(crate) fn clear() {
    publish(None);
}

pub(crate) struct Monitor {
    lease: MonitorLease,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.task.abort();
        self.lease.revoke(clear);
    }
}
pub(crate) fn start(
    broker: Weak<crate::worker::agent::computer_use_broker::ComputerUseBroker>,
    lease: MonitorLease,
) -> Monitor {
    Monitor {
        lease: lease.clone(),
        task: tokio::spawn(async move {
            loop {
                if broker.upgrade().is_none() || !lease.current() {
                    break;
                }
                let _ = watch(&broker, &lease).await;
                lease.with_current(|| {
                    clear();
                    if let Some(broker) = broker.upgrade() {
                        broker.invalidate_linux_ui();
                    }
                });
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }),
    }
}

async fn watch(
    broker: &Weak<crate::worker::agent::computer_use_broker::ComputerUseBroker>,
    lease: &MonitorLease,
) -> Result<()> {
    let (bus, subscribed) = tokio::time::timeout(Duration::from_secs(6), async {
        let bus = Bus::connect().await?;
        let subscribed = subscription::subscribe(&bus.connection).await?;
        Ok::<_, AgentError>((bus, subscribed))
    })
    .await
    .map_err(|_| error("AT-SPI lifecycle subscription timed out"))??;
    lease
        .with_current(|| publish(Some(bus.id.clone())))
        .ok_or_else(|| error("Desktop monitor was replaced"))?;
    events::observe(
        subscribed.events,
        || async {
            if !lease.current() {
                return Err(error("Desktop monitor was replaced"));
            }
            let current = Bus::connect().await?;
            if current.id != bus.id
                || current.desktop != bus.desktop
                || subscription::owner(&current.connection).await? != subscribed.owner
            {
                return Err(error("AT-SPI or desktop identity changed"));
            }
            Ok(())
        },
        || {
            lease
                .with_current(|| {
                    let broker = broker
                        .upgrade()
                        .ok_or_else(|| error("Desktop worker ended"))?;
                    publish(Some(bus.id.clone()));
                    broker.invalidate_linux_ui();
                    Ok(())
                })
                .ok_or_else(|| error("Desktop monitor was replaced"))?
        },
        Duration::from_secs(2),
        Duration::from_secs(6),
    )
    .await
}
