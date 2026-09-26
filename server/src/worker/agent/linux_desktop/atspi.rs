//! Bounded AT-SPI reads on the existing native executor, without enabling accessibility.
mod actions;
pub(crate) mod lifetime;
#[cfg(test)]
mod live_gtk;
use crate::worker::agent::{
    computer_use_broker::{CollectedUiNode, CollectedUiTree, ObservedApplication},
    native_ui_identity,
};
pub(crate) use actions::{apply_action, preflight_action};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{UiInspectParams, UiNodeLocation},
};
use std::{collections::HashSet, future::Future, time::Duration};
use zbus::{Connection, Proxy, zvariant::OwnedObjectPath};

type Object = (String, OwnedObjectPath);
type Result<T> = std::result::Result<T, AgentError>;
fn error(message: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: message.to_string(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn bounded<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(error)?;
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(6), future)
            .await
            .map_err(|_| error("AT-SPI observation deadline exceeded"))?
    })
}

struct Bus {
    connection: Connection,
    id: String,
    desktop: super::DesktopIdentity,
}
struct Application {
    object: Object,
    pid: u32,
    started: u64,
    executable: String,
}

impl Bus {
    async fn connect() -> Result<Self> {
        let desktop = super::resolve().await.map_err(error)?;
        let session = Connection::session().await.map_err(error)?;
        let status = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status")
            .await
            .map_err(error)?;
        if !status
            .get_property::<bool>("IsEnabled")
            .await
            .map_err(error)?
        {
            return Err(error(
                "Accessibility is disabled; enable it locally before using UI observation",
            ));
        }
        let locator = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
            .await
            .map_err(error)?;
        let address: String = locator.call("GetAddress", &()).await.map_err(error)?;
        let connection = zbus::connection::Builder::address(address.as_str())
            .map_err(error)?
            .build()
            .await
            .map_err(error)?;
        let dbus = Proxy::new(
            &connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await
        .map_err(error)?;
        let id = dbus.call("GetId", &()).await.map_err(error)?;
        Ok(Self {
            connection,
            id,
            desktop,
        })
    }
    async fn proxy<'a>(&'a self, object: &'a Object) -> Result<Proxy<'a>> {
        Proxy::new(
            &self.connection,
            object.0.as_str(),
            object.1.as_str(),
            "org.a11y.atspi.Accessible",
        )
        .await
        .map_err(error)
    }
    async fn applications(&self) -> Result<Vec<Application>> {
        let root = (
            "org.a11y.atspi.Registry".into(),
            OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root").map_err(error)?,
        );
        let children: Vec<Object> = self
            .proxy(&root)
            .await?
            .call("GetChildren", &())
            .await
            .map_err(error)?;
        if children.len() > 256 {
            return Err(error("AT-SPI application catalog exceeds its bound"));
        }
        let dbus = Proxy::new(
            &self.connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await
        .map_err(error)?;
        let mut apps = Vec::new();
        for (name, path) in children {
            let owner: String = dbus.call("GetNameOwner", &(&name,)).await.map_err(error)?;
            let uid: u32 = dbus
                .call("GetConnectionUnixUser", &(&owner,))
                .await
                .map_err(error)?;
            if uid != self.desktop.uid {
                continue;
            }
            let pid: u32 = dbus
                .call("GetConnectionUnixProcessID", &(&owner,))
                .await
                .map_err(error)?;
            let (executable, started) = process(pid)?;
            apps.push(Application {
                object: (owner, path),
                pid,
                started,
                executable,
            });
        }
        Ok(apps)
    }
    async fn unchanged(&self) -> Result<()> {
        if super::resolve().await.map_err(error)? != self.desktop {
            return Err(error("Desktop changed during AT-SPI observation"));
        }
        Ok(())
    }
}

fn process(pid: u32) -> Result<(String, u64)> {
    use std::os::unix::fs::MetadataExt;
    let root = format!("/proc/{pid}");
    if std::fs::metadata(&root).map_err(error)?.uid() != unsafe { libc::geteuid() } {
        return Err(error("AT-SPI process belongs to another user"));
    }
    let executable = std::fs::read_link(format!("{root}/exe"))
        .map_err(error)?
        .to_string_lossy()
        .into_owned();
    let stat = std::fs::read_to_string(format!("{root}/stat")).map_err(error)?;
    let started = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| error("AT-SPI process identity unavailable"))?;
    Ok((executable, started))
}

pub(in crate::worker::agent) fn running_applications() -> Result<Vec<ObservedApplication>> {
    native_ui_identity::run(|| {
        bounded(async {
            let bus = Bus::connect().await?;
            let epoch = lifetime::epoch(&bus.id)?;
            let applications = bus
                .applications()
                .await?
                .into_iter()
                .map(|app| ObservedApplication {
                    // No native window handle is available or used by this provider.
                    window_handle: 0,
                    process_id: app.pid,
                    image_path: app.executable,
                    process_started_at: Some(app.started),
                })
                .collect();
            bus.unchanged().await?;
            if lifetime::epoch(&bus.id)? != epoch {
                return Err(error(
                    "AT-SPI application catalog changed during inspection",
                ));
            }
            Ok(applications)
        })
    })
}

pub(in crate::worker::agent) fn application_by_pid(pid: u32) -> Result<ObservedApplication> {
    running_applications()?
        .into_iter()
        .find(|app| app.process_id == pid)
        .ok_or_else(|| error("Selected AT-SPI application is unavailable"))
}

fn state(states: &[u32], index: usize) -> bool {
    states
        .get(index / 32)
        .is_some_and(|bits| bits & (1 << (index % 32)) != 0)
}

pub(in crate::worker::agent) fn collect(
    pid: u32,
    executable: String,
    started: Option<u64>,
    params: UiInspectParams,
    selection: Option<String>,
) -> Result<CollectedUiTree> {
    native_ui_identity::run(move || {
        bounded(async move {
            let bus = Bus::connect().await?;
            let epoch = lifetime::epoch(&bus.id)?;
            let app = bus
                .applications()
                .await?
                .into_iter()
                .find(|app| {
                    app.pid == pid && app.executable == executable && Some(app.started) == started
                })
                .ok_or_else(|| error("AT-SPI application identity changed"))?;
            let selection = params
                .query
                .as_ref()
                .and_then(|query| query.element_id.clone())
                .or(selection);
            let mut stack = vec![(app.object.clone(), 0u16, None, selection.is_none(), false)];
            let mut seen = HashSet::new();
            let mut nodes = Vec::new();
            let mut bytes = 0usize;
            let mut visited = 0usize;
            let mut truncated = false;
            while let Some((object, depth, parent_index, in_selection, in_menu)) = stack.pop() {
                if object.0 != app.object.0 || !seen.insert(object.clone()) {
                    continue;
                }
                visited += 1;
                if visited > 4096 || nodes.len() >= params.max_nodes.min(512) as usize {
                    truncated = true;
                    break;
                }
                let accessible = bus.proxy(&object).await?;
                let states: Vec<u32> = accessible.call("GetState", &()).await.map_err(error)?;
                if state(&states, 6) {
                    continue;
                }
                let role: String = accessible.call("GetRoleName", &()).await.map_err(error)?;
                let protected = role == "password text";
                let name = if protected {
                    None
                } else {
                    Some(
                        accessible
                            .get_property::<String>("Name")
                            .await
                            .map_err(error)?
                            .chars()
                            .take(1024)
                            .collect::<String>(),
                    )
                };
                let fingerprint = serde_json::to_string(&(
                    bus.desktop.binding(),
                    &bus.id,
                    &object.0,
                    app.pid,
                    app.started,
                    object.1.as_str(),
                    epoch,
                ))
                .map_err(error)?;
                let selected_root = selection
                    .as_ref()
                    .is_some_and(|selected| selected == &fingerprint);
                let selected = in_selection || selected_root;
                let depth = if selected_root { 0 } else { depth };
                let in_menu = in_menu
                    || matches!(
                        role.as_str(),
                        "menu" | "menu bar" | "menu item" | "check menu item" | "radio menu item"
                    );
                let scope_match = match params.scope {
                    desk_agent_protocol::computer_use::UiInspectScope::All => true,
                    desk_agent_protocol::computer_use::UiInspectScope::Content => !in_menu,
                    desk_agent_protocol::computer_use::UiInspectScope::Menus => in_menu,
                };
                let matches = params.query.as_ref().is_none_or(|query| {
                    query.queries.is_empty()
                        || query.queries.iter().any(|term| {
                            name.as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&term.to_lowercase())
                                || role.to_lowercase().contains(&term.to_lowercase())
                        })
                });
                let mut next_parent = parent_index;
                if selected && matches && scope_match && (depth == 0 || state(&states, 25)) {
                    let supported_actions =
                        actions::supported(&bus, &object, &role, &states).await?;
                    let node = CollectedUiNode {
                        location: UiNodeLocation::default(),
                        window_fingerprint: None,
                        is_collection: false,
                        native_id: None,
                        parent_index,
                        role,
                        name,
                        value: None,
                        is_protected: protected,
                        enabled: state(&states, 8),
                        supported_actions,
                        fingerprint,
                    };
                    bytes += serde_json::to_vec(&node).map_err(error)?.len();
                    if bytes > params.max_bytes as usize {
                        truncated = true;
                        break;
                    }
                    next_parent = Some(nodes.len() as u32);
                    nodes.push(node);
                    if params.element_only && selection.is_some() {
                        break;
                    }
                }
                if ((!selected && depth < 32) || (selected && depth < params.max_depth.min(32)))
                    && !protected
                {
                    let children: Vec<Object> =
                        accessible.call("GetChildren", &()).await.map_err(error)?;
                    if children.len() > 4096 {
                        truncated = true;
                        continue;
                    }
                    for child in children.into_iter().rev() {
                        stack.push((child, depth + 1, next_parent, selected, in_menu));
                    }
                }
            }
            if process(pid)? != (app.executable, app.started) {
                return Err(error("AT-SPI application restarted during inspection"));
            }
            bus.unchanged().await?;
            if lifetime::epoch(&bus.id)? != epoch {
                return Err(error("AT-SPI tree changed during inspection"));
            }
            Ok(CollectedUiTree { nodes, truncated })
        })
    })
}
