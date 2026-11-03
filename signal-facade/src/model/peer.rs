use desk_agent_protocol::ai_assistant::AiAssistantSettings;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::{os::OperationSystemEnum, signal::RemoteDeskTypeEnum, version::VersionInfo};

/// Public peer description. The authenticated boundary decides role and browser name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, ToSchema)]
pub struct PeerInfo {
    pub remote_desk_type: RemoteDeskTypeEnum,
    pub operation_system: OperationSystemEnum,
    pub display_name: Option<String>,
}

/// Effective host execution capabilities, independent of API version and authority.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, ToSchema)]
pub struct HostExecCapabilities {
    pub available_exec_shells: Vec<String>,
    pub exec_pty: bool,
    pub exec_pty_elevation: bool,
    pub max_ai_command_runtime_ms: Option<u32>,
}

/// Private node registration, consumed before publishing the public connection model.
#[derive(Clone)]
pub struct ServerRegistration {
    pub client_id: String,
    pub exec_capabilities: HostExecCapabilities,
    pub ai_assistant_settings: AiAssistantSettings,
}

/// Inputs to the shared handler after the host controller adjudicates identity.
#[derive(Clone)]
pub struct SignalingPeer {
    pub version_info: Option<VersionInfo>,
    pub peer: PeerInfo,
    pub server: Option<ServerRegistration>,
}

impl SignalingPeer {
    pub fn browser(version_info: Option<VersionInfo>, peer: PeerInfo) -> Self {
        Self {
            version_info,
            peer,
            server: None,
        }
    }
}

/// Flat URL-query adapter only. It never contains a credential and is not stored.
#[derive(Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PeerQuery {
    pub remote_desk_type: Option<RemoteDeskTypeEnum>,
    pub operation_system: Option<OperationSystemEnum>,
    pub display_name: Option<String>,
    pub client_id: Option<String>,
    pub available_exec_shells: Option<String>,
    pub exec_pty: Option<bool>,
    pub exec_pty_elevation: Option<bool>,
    pub max_ai_command_runtime_ms: Option<u32>,
    pub ai_assistant_revision: Option<u64>,
    pub ai_assistant_enabled: Option<bool>,
}

impl PeerQuery {
    pub fn into_peer(
        self,
        version_info: Option<VersionInfo>,
        role: RemoteDeskTypeEnum,
        browser_name: Option<String>,
    ) -> Result<SignalingPeer, &'static str> {
        let peer = PeerInfo {
            remote_desk_type: role,
            operation_system: self.operation_system.unwrap_or(OperationSystemEnum::Other),
            display_name: if role == RemoteDeskTypeEnum::Browser {
                browser_name
            } else {
                self.display_name
            },
        };
        let server = if role == RemoteDeskTypeEnum::Server {
            if version_info.is_none() {
                return Err("Missing node version");
            }
            let client_id = self
                .client_id
                .filter(|id| !id.trim().is_empty())
                .ok_or("Missing device identity")?;
            let exec_pty = self.exec_pty.ok_or("Missing node capabilities")?;
            let exec_pty_elevation = self.exec_pty_elevation.ok_or("Missing node capabilities")?;
            if exec_pty_elevation && !exec_pty {
                return Err("Invalid node capabilities");
            }
            Some(ServerRegistration {
                client_id,
                exec_capabilities: HostExecCapabilities {
                    available_exec_shells: self
                        .available_exec_shells
                        .ok_or("Missing node capabilities")?
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    exec_pty,
                    exec_pty_elevation,
                    max_ai_command_runtime_ms: Some(
                        self.max_ai_command_runtime_ms
                            .ok_or("Missing node limits")?,
                    ),
                },
                ai_assistant_settings: AiAssistantSettings {
                    revision: self
                        .ai_assistant_revision
                        .ok_or("Missing device settings")?,
                    enabled: self.ai_assistant_enabled.ok_or("Missing device settings")?,
                },
            })
        } else {
            None
        };
        Ok(SignalingPeer {
            version_info,
            peer,
            server,
        })
    }
}

impl SignalingPeer {
    /// Encode public reports and private registration at the URL boundary.
    /// The caller appends its credential separately.
    pub fn to_query_string(&self) -> Result<String, serde_urlencoded::ser::Error> {
        let server = self.server.as_ref();
        let query = PeerQuery {
            remote_desk_type: Some(self.peer.remote_desk_type),
            operation_system: Some(self.peer.operation_system),
            display_name: self.peer.display_name.clone(),
            client_id: server.map(|s| s.client_id.clone()),
            available_exec_shells: server
                .map(|s| s.exec_capabilities.available_exec_shells.join(",")),
            exec_pty: server.map(|s| s.exec_capabilities.exec_pty),
            exec_pty_elevation: server.map(|s| s.exec_capabilities.exec_pty_elevation),
            max_ai_command_runtime_ms: server
                .and_then(|s| s.exec_capabilities.max_ai_command_runtime_ms),
            ai_assistant_revision: server.map(|s| s.ai_assistant_settings.revision),
            ai_assistant_enabled: server.map(|s| s.ai_assistant_settings.enabled),
        };
        let peer = serde_urlencoded::to_string(&query)?;
        match &self.version_info {
            Some(version) => Ok(format!("{}&{peer}", serde_urlencoded::to_string(version)?)),
            None => Ok(peer),
        }
    }
}

impl SignalingPeer {
    /// Consume registration once, separating private audience from public reports.
    pub fn into_connection_model(
        self,
        connection_id: String,
        ip: Option<String>,
    ) -> (Option<String>, super::connection::ConnectionModel) {
        let (client_id, exec_capabilities, ai_assistant_settings) = match self.server {
            Some(server) => (
                Some(server.client_id),
                server.exec_capabilities,
                Some(server.ai_assistant_settings),
            ),
            None => (None, HostExecCapabilities::default(), None),
        };
        let model = super::connection::ConnectionModel {
            connection_id,
            ip,
            version_info: self.version_info,
            peer: self.peer,
            exec_capabilities,
            ai_assistant_settings,
            device_id: client_id.clone(),
            owner_node_id: None,
        };
        (client_id, model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "remote_desk_type=server&operation_system=Linux&display_name=Desk&client_id=stable-host&available_exec_shells=bash,zsh&exec_pty=true&exec_pty_elevation=true&max_ai_command_runtime_ms=12345&ai_assistant_revision=8&ai_assistant_enabled=true";
    fn version() -> VersionInfo {
        VersionInfo {
            api_version: 17,
            build_number: 42,
            commit_hash: "source".into(),
            debug_build: false,
        }
    }
    fn query(raw: &str) -> PeerQuery {
        serde_urlencoded::from_str(raw).unwrap()
    }

    #[test]
    fn registration_round_trip_preserves_capabilities_and_consumes_private_identity() {
        let peer = query(HOST)
            .into_peer(Some(version()), RemoteDeskTypeEnum::Server, None)
            .unwrap();
        let encoded = peer.to_query_string().unwrap();
        assert!(!encoded.contains("token"));
        let version: VersionInfo = serde_urlencoded::from_str(&encoded).unwrap();
        let decoded = query(&encoded)
            .into_peer(Some(version), RemoteDeskTypeEnum::Server, None)
            .unwrap();
        let (audience, model) = decoded.into_connection_model("connection".into(), None);
        assert_eq!(audience.as_deref(), Some("stable-host"));
        assert_eq!(model.device_id.as_deref(), Some("stable-host"));
        assert_eq!(
            model.exec_capabilities.available_exec_shells,
            ["bash", "zsh"]
        );
        assert!(model.exec_capabilities.exec_pty && model.exec_capabilities.exec_pty_elevation);
        assert_eq!(
            model.exec_capabilities.max_ai_command_runtime_ms,
            Some(12345)
        );
        assert_eq!(
            model.ai_assistant_settings,
            Some(AiAssistantSettings {
                revision: 8,
                enabled: true
            })
        );
        let json = serde_json::to_value(&model).unwrap();
        let mut version_fields = json["version_info"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        version_fields.sort_unstable();
        assert_eq!(
            version_fields,
            ["api_version", "build_number", "commit_hash", "debug_build"]
        );
        assert!(json.get("client_id").is_none());
        assert!(json.get("token").is_none());
    }

    #[test]
    fn cookie_identity_discards_node_registration_and_never_invents_client_version_or_os() {
        let peer = query("remote_desk_type=server&client_id=forged&display_name=forged")
            .into_peer(None, RemoteDeskTypeEnum::Browser, Some("account".into()))
            .unwrap();
        assert!(peer.version_info.is_none());
        assert!(peer.server.is_none());
        assert_eq!(peer.peer.remote_desk_type, RemoteDeskTypeEnum::Browser);
        assert_eq!(peer.peer.operation_system, OperationSystemEnum::Other);
        assert_eq!(peer.peer.display_name.as_deref(), Some("account"));
    }

    #[test]
    fn server_registration_requires_complete_reports_and_consistent_pty_capabilities() {
        assert!(
            query(HOST)
                .into_peer(None, RemoteDeskTypeEnum::Server, None)
                .is_err()
        );
        for key in [
            "client_id",
            "available_exec_shells",
            "exec_pty",
            "exec_pty_elevation",
            "max_ai_command_runtime_ms",
            "ai_assistant_revision",
            "ai_assistant_enabled",
        ] {
            let fields = HOST
                .split('&')
                .filter(|field| !field.starts_with(&format!("{key}=")))
                .collect::<Vec<_>>()
                .join("&");
            assert!(
                query(&fields)
                    .into_peer(Some(version()), RemoteDeskTypeEnum::Server, None)
                    .is_err(),
                "{key}"
            );
        }
        assert!(
            query(&HOST.replace("client_id=stable-host", "client_id=%20"))
                .into_peer(Some(version()), RemoteDeskTypeEnum::Server, None)
                .is_err()
        );
        assert!(
            query(&HOST.replace("exec_pty=true", "exec_pty=false"))
                .into_peer(Some(version()), RemoteDeskTypeEnum::Server, None)
                .is_err()
        );
    }
}
