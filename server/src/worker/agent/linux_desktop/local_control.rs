//! A same-user native connection owns one bounded best-effort control period.
//! No HTTP, remote tool, permission elevation or input-event content is exposed.
mod cli;
mod client;
pub use client::{InputControlCancellation, InputControlReport, NativeInputControlClient};
#[cfg(test)]
mod tests;
pub use cli::run as run_cli;

use super::monitor_owner::MonitorLease;
use crate::worker::agent::computer_use_broker::ComputerUseBroker;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::{io, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::JoinSet,
};

const MAX_MESSAGE: usize = 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    duration_secs: u16,
    accept_partial: bool,
}
impl Request {
    fn validate(&self) -> io::Result<()> {
        if self.version != 1 || !(1..=300).contains(&self.duration_secs) {
            return Err(io::Error::other(
                "Unsupported protocol or duration; use 1..300 seconds",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Response {
    Active {
        grabbed: usize,
        failed: usize,
        skipped: usize,
    },
    Unavailable {
        reason: String,
    },
    Ended,
}

pub(crate) struct Endpoint {
    task: tokio::task::JoinHandle<()>,
    path: PathBuf,
    hub: Option<Arc<crate::host_control::HostControlHub>>,
    daemon: Option<tokio::sync::mpsc::UnboundedSender<desk_ipc_protocol::message::WorkerToService>>,
}
impl Endpoint {
    pub(crate) fn announce_worker(
        &mut self,
        sender: tokio::sync::mpsc::UnboundedSender<desk_ipc_protocol::message::WorkerToService>,
    ) {
        let _ = sender.send(
            desk_ipc_protocol::message::WorkerToService::LinuxAiInputEndpoint(Some(
                self.path.to_string_lossy().into_owned(),
            )),
        );
        self.daemon = Some(sender);
    }

    pub(crate) fn publish(&mut self, hub: Arc<crate::host_control::HostControlHub>) {
        hub.publish_linux_ai_input_endpoint(self.path.to_string_lossy().into_owned());
        self.hub = Some(hub);
    }
    pub(crate) fn start(broker: Arc<ComputerUseBroker>, owner: MonitorLease) -> io::Result<Self> {
        let uid = unsafe { libc::geteuid() };
        if uid == 0 || uid != unsafe { libc::getuid() } {
            return Err(io::Error::other(
                "Input control requires an unelevated user worker",
            ));
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is missing"))?;
        if !runtime.is_absolute() {
            return Err(io::Error::other("Runtime path is not absolute"));
        }
        private_directory(&runtime, uid)?;
        let directory = runtime.join("lcxl-ai-input");
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        private_directory(&directory, uid)?;
        // Each worker lifetime gets its own endpoint; never unlink another
        // instance's socket or infer ownership from a recycled PID.
        let path = directory.join(format!("{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&path)?;
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        log::info!(
            "Local AI best-effort input control endpoint: {}",
            path.display()
        );
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        if clients.len() >= 4 || stream.peer_cred().map(|p| p.uid()).ok() != Some(uid) {
                            continue;
                        }
                        let broker = broker.clone();
                        let owner = owner.clone();
                        clients.spawn(async move {
                            if let Err(error) = serve(stream, broker, owner).await {
                                log::debug!("Local AI input control connection ended: {error}");
                            }
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Ok(Self {
            task,
            path,
            hub: None,
            daemon: None,
        })
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        // JoinSet drops/aborts the connection tasks and their lease guards.
        // A still-running blocking acquisition returns an owned lease whose
        // drop releases it even when its async receiver was cancelled.
        if let Some(hub) = &self.hub {
            hub.clear_linux_ai_input_endpoint(&self.path.to_string_lossy());
        }
        if let Some(sender) = &self.daemon {
            let _ = sender
                .send(desk_ipc_protocol::message::WorkerToService::LinuxAiInputEndpoint(None));
        }
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}
fn private_directory(path: &std::path::Path, uid: u32) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "Input control requires a private user-owned runtime directory",
        ));
    }
    Ok(())
}

struct Lease {
    broker: Arc<ComputerUseBroker>,
    generation: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.broker.end_linux_input_control(self.generation);
    }
}
trait ActiveControl {
    fn active(&self) -> bool;
}
impl ActiveControl for Lease {
    fn active(&self) -> bool {
        self.broker
            .require_linux_input_control(Some(self.generation))
            .is_ok()
    }
}

async fn write(stream: &mut UnixStream, response: &Response) -> io::Result<()> {
    let bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::other("Control response exceeds bound"));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        stream.write_u32(bytes.len() as u32).await?;
        stream.write_all(&bytes).await
    })
    .await
    .map_err(|_| io::Error::other("Control response timed out"))?
}
async fn serve(
    mut stream: UnixStream,
    broker: Arc<ComputerUseBroker>,
    owner: MonitorLease,
) -> io::Result<()> {
    let request: Request = tokio::time::timeout(Duration::from_secs(5), async {
        let length = stream.read_u32().await? as usize;
        if length == 0 || length > MAX_MESSAGE {
            return Err(io::Error::other("Control request exceeds bound"));
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).await?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    })
    .await
    .map_err(|_| io::Error::other("Control request timed out"))??;
    request.validate()?;
    let admitted = tokio::task::spawn_blocking(move || {
        owner
            .with_current(|| {
                let receipt = broker.begin_linux_input_control(
                    Duration::from_secs(u64::from(request.duration_secs)),
                    request.accept_partial,
                )?;
                Ok::<_, io::Error>((
                    Lease {
                        broker,
                        generation: receipt.generation,
                    },
                    receipt.report,
                ))
            })
            .unwrap_or_else(|| Err(io::Error::other("Worker was replaced")))
    })
    .await
    .map_err(io::Error::other)?;
    let (lease, report) = match admitted {
        Ok(value) => value,
        Err(error) => {
            return write(
                &mut stream,
                &Response::Unavailable {
                    reason: error.to_string(),
                },
            )
            .await;
        }
    };
    hold_connection(stream, lease, report).await
}

async fn hold_connection<G: ActiveControl>(
    mut stream: UnixStream,
    lease: G,
    report: desk_input_injection::linux_input_block::BlockReport,
) -> io::Result<()> {
    if !lease.active() {
        drop(lease);
        return write(&mut stream, &Response::Ended).await;
    }
    write(
        &mut stream,
        &Response::Active {
            grabbed: report.grabbed,
            failed: report.failed,
            skipped: report.skipped,
        },
    )
    .await?;
    let mut byte = [0];
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            // EOF, an explicit stop byte, or a read error all end this lease.
            _ = stream.read(&mut byte) => break,
            _ = tick.tick() => {
                if !lease.active() {
                    break;
                }
            }
        }
    }
    // Release before acknowledging completion. A failed write also drops it.
    drop(lease);
    write(&mut stream, &Response::Ended).await
}
