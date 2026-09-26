//! Subscribe before resolving identity; any relevant transition fences old refs.
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::{StreamExt, stream::SelectAll};
use zbus::{Connection, MessageStream};

use super::super::computer_use_broker::ComputerUseBroker;
use super::monitor_owner::MonitorLease;

pub(crate) struct DesktopMonitor {
    _atspi: super::atspi::lifetime::Monitor,
    task: tokio::task::JoinHandle<()>,
    broker: Weak<ComputerUseBroker>,
    lease: MonitorLease,
}

impl DesktopMonitor {
    pub(crate) fn owner_lease(&self) -> MonitorLease {
        self.lease.clone()
    }

    pub(crate) fn start(broker: &Arc<ComputerUseBroker>) -> Self {
        let lease = broker.linux_monitor_owner.claim();
        lease.with_current(|| {
            broker.set_linux_desktop_identity(None);
            super::atspi::lifetime::clear();
        });
        let task_lease = lease.clone();
        let owner = Arc::downgrade(broker);
        let weak = owner.clone();
        let task = tokio::spawn(async move {
            loop {
                if weak.upgrade().is_none() || !task_lease.current() {
                    return;
                }
                let result = watch(&weak, &task_lease).await;
                if let Some(broker) = weak.upgrade() {
                    task_lease.with_current(|| broker.set_linux_desktop_identity(None));
                } else {
                    return;
                }
                if let Err(error) = result {
                    log::debug!("GNOME desktop identity monitor unavailable: {error}");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        Self {
            _atspi: super::atspi::lifetime::start(owner.clone(), lease.clone()),
            task,
            broker: owner,
            lease,
        }
    }
}

impl Drop for DesktopMonitor {
    fn drop(&mut self) {
        self.task.abort();
        self.lease.revoke(|| {
            super::atspi::lifetime::clear();
            if let Some(broker) = self.broker.upgrade() {
                broker.set_linux_desktop_identity(None);
            }
        });
    }
}

async fn watch(weak: &Weak<ComputerUseBroker>, lease: &MonitorLease) -> Result<(), zbus::Error> {
    let system = Connection::system().await?;
    let session = Connection::session().await?;
    let mut events = SelectAll::new();
    for (connection, rule) in [
        (
            &system,
            "type='signal',sender='org.freedesktop.login1',path_namespace='/org/freedesktop/login1'",
        ),
        (
            &system,
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.freedesktop.login1'",
        ),
        (
            &session,
            "type='signal',sender='org.gnome.ScreenSaver',interface='org.gnome.ScreenSaver',member='ActiveChanged'",
        ),
        (
            &session,
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.gnome.Shell'",
        ),
        (
            &session,
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.gnome.ScreenSaver'",
        ),
    ] {
        let stream = MessageStream::for_match_rule(rule, connection, Some(64)).await?;
        events.push(
            stream
                .chain(futures_util::stream::once(async {
                    Err(zbus::Error::Failure(
                        "Desktop event subscription ended".into(),
                    ))
                }))
                .boxed(),
        );
    }
    loop {
        if !lease.current() {
            return Ok(());
        }
        // Continue servicing invalidation while D-Bus identity reads are pending.
        let resolved = tokio::select! {
            biased;
            event = events.next() => {
                if let Some(broker) = weak.upgrade() { lease.with_current(|| broker.set_linux_desktop_identity(None)); }
                match event { Some(Ok(_)) => continue, Some(Err(error)) => return Err(error), None => return Ok(()) }
            }
            identity = super::resolve() => identity,
        };
        let Some(broker) = weak.upgrade() else {
            return Ok(());
        };
        if lease
            .with_current(|| broker.set_linux_desktop_identity(resolved.ok()))
            .is_none()
        {
            return Ok(());
        }
        drop(broker);
        // Also detect socket/process replacement and transport loss without a
        // compositor signal. Each actual desktop operation must revalidate too.
        tokio::select! {
            biased;
            event = events.next() => {
                if let Some(broker) = weak.upgrade() { lease.with_current(|| broker.set_linux_desktop_identity(None)); }
                match event { Some(Ok(_)) => {}, Some(Err(error)) => return Err(error), None => return Ok(()) }
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}
