pub mod controller;
pub mod daemon;
pub mod diagnose;
pub mod durable_file;
pub mod error;
pub mod exec;
pub mod exec_shells;
pub mod host_activity;
pub mod host_control;
pub mod locale;
#[cfg(target_os = "macos")]
pub mod macos_agent;
#[cfg(target_os = "macos")]
pub mod macos_autologin;
#[cfg(target_os = "macos")]
pub mod macos_permissions;
pub mod mcp;
pub mod model;
pub mod openapi;
pub mod service;
pub mod telemetry;
pub mod transport_guard;
pub mod version;
pub mod worker;

use std::{
    collections::BTreeMap,
    env,
    fs::{File, TryLockError},
    io::ErrorKind,
    path::Path,
    sync::Arc,
};

use crate::{
    controller::{
        api_token::create_token,
        connection::verify_connection,
        info::{query_backend_info, query_macos_autologin, query_server_info, query_sysinfo},
        init::init_system,
        login::{change_password, get_captcha, login_account, login_tauri, logout_account},
        manager_link::{query_manager_link_status, retry_manager_link},
        redeem::redeem_code,
        service_mgmt::{install_service, uninstall_service},
        settings::{
            ack_security_approval, query_ai_policy_settings, query_collection_policy_settings,
            query_log_settings, query_security_settings, query_settings, query_telemetry_status,
            query_turn_client_settings, query_turn_settings, regenerate_turn_secret,
            submit_security_approval, update_ai_policy_settings, update_collection_policy_settings,
            update_log_settings, update_security_settings, update_settings,
            update_telemetry_consent, update_turn_client_settings, update_turn_settings,
        },
        support::{start_support, stop_support, support_status},
        turn::{
            delete_turn_session, get_turn_info, get_turn_metrics, get_turn_session,
            get_turn_session_statistics,
        },
        user::{enforce_device_scope, get_current_user},
        virtual_display::{
            install_driver as install_virtual_display_driver,
            query_driver_status as query_virtual_display_driver_status,
            query_virtual_display_settings, uninstall_driver as uninstall_virtual_display_driver,
            update_virtual_display_settings,
        },
    },
    service::{
        turn_lifecycle::{mode_hosts_turn, turn_plan},
        turn_runtime::{HostTurnDriver, TurnRuntimeControl, TurnRuntimeStopGuard},
    },
};
use actix_server::Server;
use actix_service::fn_service;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{
    App, HttpResponse, HttpServer,
    cookie::Key,
    dev::Service as _,
    dev::{ServiceRequest, ServiceResponse},
    error::InternalError,
    middleware::{Logger, from_fn},
    web::{self},
};
use clap::Parser as _;
use desk_signal::{
    controller::{
        ai_usage::get_model_usage,
        connection::list_connections,
        device_code::{
            batch_delete_device_codes, create_device_code, delete_device_code, list_device_codes,
            update_device_code,
        },
        diagnose_session::{get_diagnose_session, list_diagnose_sessions},
        model_provider::{get_model_provider, test_model_provider, update_model_provider},
        signaling::open_signaling_handle,
        terminal::{list_terminal, open_terminal_session},
        turn_usage::get_turn_usage,
        usage_retention::{get_usage_retention, update_usage_retention},
    },
    model::SharedConnectionMap,
};
use desk_turn::runtime::{TurnIntent, TurnRuntimeView};
use desk_utils::{error::DeskErrorCode, network::check_ipv6_available, rest::RestResponse};
use error::DeskError;
use log::{error, info, warn};
use model::settings::{Args, Settings, SharedSettings, StartupMode};
use tracing_appender::non_blocking::WorkerGuard;

use utoipa::OpenApi;
use utoipa_actix_web::AppExt;

rust_i18n::i18n!("locales");

/// Shared override for the `is_admin` field in `/api/server_info` and `/api/desk/sysinfo`.
/// In ServiceDaemon mode the process runs as SYSTEM so `is_admin()` is always true.
/// The Tauri shell reports its own admin status via the IPC WebSocket; that value is stored
/// here and read by the info controllers so the frontend receives the correct elevation status.
/// `None` = use platform `is_admin()` directly (portable / non-daemon modes).
pub type TauriIsAdminOverride = std::sync::Arc<std::sync::Mutex<Option<bool>>>;

/// Parameters for registering the core HTTP API routes.
/// All fields are `Clone` so this struct can be captured by `HttpServer::new` closures.
#[derive(Clone)]
pub struct ApiRouteConfig {
    pub settings: web::Data<SharedSettings>,
    pub tauri_login_token: web::Data<Option<TauriLoginToken>>,
    pub connection_map: web::Data<desk_signal::model::SharedConnectionMap>,
    /// Optional unified host-control hub. Wired into the security-approval submit
    /// endpoint and (in Aggregator mode) the host-control ws routes.
    pub host_control_hub: web::Data<Option<Arc<host_control::HostControlHub>>>,
    pub tauri_is_admin: Option<web::Data<TauriIsAdminOverride>>,
}

/// Selects *which* routes [`configure_api_surface`] registers. Pure route-set
/// selection — carries no runtime `app_data` (callers mount `Data` on their own
/// `App`), so it stays a clean single source of truth for the HTTP surface.
#[derive(Clone, Copy, Debug)]
pub struct ApiSurfaceOpts {
    /// Register the signaling WS handle (top level, bypasses `enforce_device_scope`).
    pub include_signaling: bool,
    /// Register file management + device-code admin under `/api/desk`.
    pub include_device_code: bool,
    /// Whether this process has an initialized local signal SQLite DB.
    ///
    /// Gates the routes whose handlers call `desk_signal::db::get_db()`, which
    /// panics when the DB was never opened. This is the only condition on the
    /// `/turn` scope: the TURN *runtime* endpoints are registered everywhere
    /// (whether a relay is running is a runtime answer, not a route-table one),
    /// while `/api/turn/usage` reads the historical rollups and needs the DB —
    /// which the ServiceDaemon has and a pure desk-server does not. Keep it in
    /// sync with [`startup_mode_has_signal_db`], which decides where the DB is
    /// opened.
    pub has_signal_db: bool,
    /// Register the `/api/model/*` scope (the collect-only token-usage view plus
    /// the central-brain model-provider config) and the usage-retention windows
    /// that govern it. Also DB-backed, so it may only be set where
    /// [`Self::has_signal_db`] is.
    pub include_model_usage: bool,
}

/// Whether a startup mode opens the local signal SQLite DB.
///
/// Single source of truth shared by the DB bootstrap and by the route gate
/// [`ApiSurfaceOpts::has_signal_db`]: the handlers behind that gate reach for
/// `desk_signal::db::get_db()`, which panics when the DB was never opened, so
/// the two conditions must never drift apart.
///
/// `ServiceDaemon` opens it in its own bootstrap (`daemon.rs`) before the local
/// API comes up; `SessionWorker` and `McpStdio` serve no HTTP API at all.
pub fn startup_mode_has_signal_db(mode: &StartupMode) -> bool {
    match mode {
        StartupMode::Default | StartupMode::Signaling | StartupMode::ServiceDaemon => true,
        StartupMode::DeskServer | StartupMode::SessionWorker | StartupMode::McpStdio => false,
    }
}

/// Single source of truth for the desk-server HTTP API surface: every
/// `#[utoipa::path]` route plus its scope nesting. The portable server
/// (`run_with_hub`), the daemon's local API (`run_local_api`), and the offline
/// [`build_openapi`] all register through this one function, so the served
/// routes and the generated OpenAPI client cannot drift apart.
///
/// Built on `utoipa_actix_web::service_config::ServiceConfig` so the same
/// registration both serves requests and feeds the OpenAPI collector.
///
/// Intentionally excluded (handled by each caller on its own `App`): `app_data`,
/// middleware, the non-utoipa host-control `/ws/*` routes, and static files.
pub fn configure_api_surface(
    cfg: &mut utoipa_actix_web::service_config::ServiceConfig,
    opts: ApiSurfaceOpts,
) {
    // Signaling (device-code lookup on handshake), device-code admin and the
    // model views all reach `desk_signal::db::get_db()`, which panics instead of
    // degrading when the DB was never opened. None of them may be registered
    // where the process has no signal DB.
    debug_assert!(
        opts.has_signal_db
            || !(opts.include_signaling || opts.include_device_code || opts.include_model_usage),
        "signal-DB-backed routes requested without a signal DB: {opts:?}",
    );

    // Public routes (no login required).
    cfg.service(login_account)
        .service(login_tauri)
        .service(logout_account)
        .service(get_current_user)
        .service(get_captcha)
        .service(query_server_info)
        .service(install_service)
        .service(uninstall_service)
        .service(init_system)
        // Connection-verify performs its own self-authentication (open before the
        // system is initialized, session-gated after), so it is registered outside
        // the `/api` scope to bypass `enforce_device_scope`, like signaling.
        .service(verify_connection);

    // Signaling WS is registered at the top level (outside the `/api` scope) so
    // it bypasses `enforce_device_scope` — the handler performs its own
    // token/session auth. It MUST be registered before the `/api` scope below.
    //
    // Code redemption is likewise a top-level public route: the redeemer is
    // anonymous (it has no owner session yet), so it must bypass the owner-only
    // guard. It mints a capability-scoped code-session, never a full session, and
    // only exists where an embedded signal (and its connection map) does. Its
    // exact `/api/desk/redeem-code` path, registered before the `/api` scope, is
    // matched here rather than by the guarded scope.
    if opts.include_signaling {
        cfg.service(open_signaling_handle).service(redeem_code);
    }

    cfg.service(
        utoipa_actix_web::scope("/api")
            .wrap(from_fn(enforce_device_scope))
            .service(query_virtual_display_driver_status)
            .service(install_virtual_display_driver)
            .service(uninstall_virtual_display_driver)
            .configure(move |cfg| {
                // Host signaling-token issuance is only meaningful where a
                // co-located signaling server exists for the host to connect to
                // (Default / Signaling / ServiceDaemon). A pure DeskServer has
                // no embedded signaling route, so it never offers this endpoint.
                if opts.include_signaling {
                    cfg.service(create_token)
                        .service(get_diagnose_session)
                        .service(list_diagnose_sessions);
                }
            })
            .service(
                utoipa_actix_web::scope("/desk")
                    .service(change_password)
                    .service(query_settings)
                    .service(update_settings)
                    .service(query_ai_policy_settings)
                    .service(update_ai_policy_settings)
                    .service(query_collection_policy_settings)
                    .service(update_collection_policy_settings)
                    .service(query_turn_settings)
                    .service(update_turn_settings)
                    .service(query_turn_client_settings)
                    .service(update_turn_client_settings)
                    .service(query_log_settings)
                    .service(update_log_settings)
                    .service(query_security_settings)
                    .service(update_security_settings)
                    .service(submit_security_approval)
                    .service(ack_security_approval)
                    .service(regenerate_turn_secret)
                    .service(query_telemetry_status)
                    .service(update_telemetry_consent)
                    .service(query_manager_link_status)
                    .service(retry_manager_link)
                    .service(start_support)
                    .service(stop_support)
                    .service(support_status)
                    .service(list_connections)
                    .service(list_terminal)
                    .service(open_terminal_session)
                    .service(query_sysinfo)
                    .service(query_backend_info)
                    .service(query_macos_autologin)
                    .service(query_virtual_display_settings)
                    .service(update_virtual_display_settings)
                    .configure(move |cfg| {
                        if opts.include_device_code {
                            cfg.service(create_device_code)
                                .service(list_device_codes)
                                .service(update_device_code)
                                .service(delete_device_code)
                                .service(batch_delete_device_codes);
                        }
                    }),
            )
            .configure(move |cfg| {
                // The `/turn` scope carries two unrelated capabilities. The
                // runtime-management endpoints are always registered: whether a
                // relay is running changes while the process runs, and a route
                // table fixed at App construction cannot track that — they answer
                // "not running, here is why" instead of vanishing. The usage
                // history only reads the local signal DB, so it stays gated on
                // having one, or `get_db()` becomes a reachable panic on a pure
                // desk-server.
                cfg.service(
                    utoipa_actix_web::scope("/turn")
                        .service(get_turn_info)
                        .service(get_turn_session)
                        .service(get_turn_session_statistics)
                        .service(delete_turn_session)
                        .service(get_turn_metrics)
                        .configure(move |cfg| {
                            if opts.has_signal_db {
                                cfg.service(get_turn_usage);
                            }
                        }),
                );
            })
            .configure(move |cfg| {
                if opts.include_model_usage {
                    cfg.service(
                        utoipa_actix_web::scope("/model")
                            .service(get_model_usage)
                            .service(get_model_provider)
                            .service(update_model_provider)
                            .service(test_model_provider),
                    )
                    // Usage-retention windows govern both rollup tables; the row
                    // lives in the same local signal DB, present whenever the
                    // usage view is.
                    .service(
                        utoipa_actix_web::scope("/usage")
                            .service(get_usage_retention)
                            .service(update_usage_retention),
                    );
                }
            }),
    );
}

/// Shared JSON extractor config used by both the portable App and the daemon's
/// local API: a 16 MiB body limit plus a uniform `RestResponse` error body on
/// malformed JSON. Mounted as `app_data` by each caller (kept out of
/// [`configure_api_surface`], which stays pure route registration).
pub fn api_json_config() -> web::JsonConfig {
    web::JsonConfig::default()
        .limit((4096 * 1024) << 2)
        .error_handler(|err, req| {
            warn!("progress request {} err: {}", req.path(), err);
            let err_message = err.to_string();
            InternalError::from_response(
                err,
                HttpResponse::BadRequest().json(RestResponse::failed(
                    DeskErrorCode::SYSTEM_ERROR,
                    err_message,
                )),
            )
            .into()
        })
}

/// Build the OpenAPI spec offline from [`configure_api_surface`] with the full
/// superset of routes, without binding a socket or touching any infrastructure
/// (no DB / Redis / HTTP / lock). Used by the `dump-openapi` CLI to regenerate
/// the typed frontend client locally and in CI. Reuses the exact same
/// registration the live servers serve, so the spec cannot drift.
/// Whether the session cookie should carry the `Secure` attribute, read from the
/// `LRD_COOKIE_SECURE` environment variable.
///
/// Defaults to `false` so the common local / LAN HTTP setup keeps working (a
/// `Secure` cookie is dropped by the browser over plain HTTP, which would lose the
/// session). A public deployment served over HTTPS (typically behind a TLS-
/// terminating reverse proxy) should set `LRD_COOKIE_SECURE=true` so the cookie is
/// only ever sent over HTTPS.
pub fn cookie_secure_from_env() -> bool {
    parse_cookie_secure(std::env::var("LRD_COOKIE_SECURE").ok().as_deref())
}

/// Parse the `LRD_COOKIE_SECURE` value, defaulting to `false` when absent or
/// unrecognized. Split out from [`cookie_secure_from_env`] so it is unit-testable
/// without mutating the process environment.
fn parse_cookie_secure(raw: Option<&str>) -> bool {
    raw.map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

pub fn build_openapi() -> utoipa::openapi::OpenApi {
    let (_app, mut api) = App::new()
        .into_utoipa_app()
        .configure(|cfg| {
            configure_api_surface(
                cfg,
                ApiSurfaceOpts {
                    include_signaling: true,
                    include_device_code: true,
                    has_signal_db: true,
                    include_model_usage: true,
                },
            )
        })
        .split_for_parts();
    api.merge(openapi::ExtraSchemas::openapi());
    api
}

/// Service management operations that can be requested by the embedded HTTP
/// server and fulfilled by the Tauri host (which has UAC elevation ability).
///
/// `install_idd_driver` rides along on the `Install` variant: when `true`,
/// the elevated sidecar will stage the LcxlVirtualDisplay IDD driver as
/// part of the install. The flag is intentionally part of the wire-level
/// command (rather than a separate REST op) so the user only has to
/// approve the UAC prompt once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOp {
    Install {
        install_path: String,
        install_idd_driver: bool,
    },
    Uninstall,
}

use std::sync::Mutex;

/// One-time token for Tauri auto-login.
/// The token is consumed after the first successful use.
///
/// The inner Mutex is Arc-wrapped so clones share the same state.
/// This allows the daemon IPC bridge and the HTTP server to share one token
/// instance and keep it in sync across Tauri reconnects.
#[derive(Clone)]
pub struct TauriLoginToken(Arc<Mutex<Option<String>>>);

impl TauriLoginToken {
    pub fn new(token: String) -> Self {
        TauriLoginToken(Arc::new(Mutex::new(Some(token))))
    }

    /// Construct in the empty (unset) state. Used when the Host Control Hub will
    /// later push a fresh token over `/ws/tauri_ipc` on every Tauri reconnect.
    pub fn empty() -> Self {
        TauriLoginToken(Arc::new(Mutex::new(None)))
    }

    /// Verify and consume the token. Returns true only once for the correct token.
    pub fn verify_and_consume(&self, candidate: &str) -> bool {
        let mut guard = self.0.lock().unwrap();
        if let Some(ref stored) = *guard {
            // Constant-time comparison to prevent timing attacks
            if constant_time_eq(stored.as_bytes(), candidate.as_bytes()) {
                *guard = None; // Consume: one-time use
                return true;
            }
        }
        false
    }

    /// Replace the stored token (used by daemon IPC bridge on Tauri reconnect).
    pub fn refresh(&self, token: String) {
        *self.0.lock().unwrap() = Some(token);
    }
}

/// Constant-time byte comparison
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Returned alongside the running [`Server`]. The caller MUST keep this guard
/// alive until `server.await` completes — dropping it earlier shuts down the
/// `tracing_appender` non-blocking writer thread, after which every log line
/// the running server emits is silently discarded.
pub type ServerHandle = (Server, Option<WorkerGuard>);

pub async fn run() -> Result<ServerHandle, DeskError> {
    let args = Args::parse();
    let settings = Settings::new(&args)?;
    run_with_hub(&settings, None).await
}

/// Run the embedded server with an optional caller-supplied
/// [`host_control::HostControlHub`].
///
/// Pass `Some(hub)` from the Tauri portable shell so business code, the embedded
/// `/ws/tauri_ipc` endpoint, and the Tauri ws client all share a single hub
/// instance. Pass `None` for headless / desk-server / signaling modes — a
/// `Local` hub is constructed internally and approval prompts deny-fast when no
/// Tauri shell is connected.
pub async fn run_with_hub(
    settings: &Settings,
    host_control_hub: Option<Arc<host_control::HostControlHub>>,
) -> Result<ServerHandle, DeskError> {
    // Create a lock file to prevent multiple instances of the server from running simultaneously.
    let lock_file_path = env::temp_dir().join("lcxl_remote_desk_server.lock");
    let lock_file = File::create(lock_file_path)?;
    if let Err(TryLockError::WouldBlock) = lock_file.try_lock() {
        error!("Failed to lock file, is another instance running?");
        return Err(DeskError::from(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "Failed to lock file, is another instance running?",
        )));
    }

    // Install default crypto provider
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    if settings.log.traceback {
        // Set RUST_BACKTRACE environment variable to 1 to enable backtraces for errors. This is useful for debugging.
        unsafe { env::set_var("RUST_BACKTRACE", "1") };
    }
    // Initialize settings
    let shared_settings = Arc::new(SharedSettings::from(settings.clone()));

    // The host's authoritative security policy, and the only path that commits
    // it or the locale durably. Every mode builds one; the modes with no worker
    // simply have nobody to publish to.
    let settings_coordinator = Arc::new(
        crate::model::settings_coordinator::SettingsCoordinator::from_settings(
            shared_settings.clone(),
        )
        .await,
    );

    // determine startup mode
    let startup_mode = settings.args.startup_mode.clone();

    // Initialize telemetry. The returned `WorkerGuard` is propagated back to
    // the caller through `ServerHandle` so it lives as long as the running
    // server. Holding it inside this function would drop it on `Ok(server)`,
    // which closes the non-blocking writer thread before any request-time
    // logs make it to disk.
    let telemetry_guard = telemetry::init_telemetry(shared_settings.clone(), &startup_mode).await?;

    // init desk_signal db
    if startup_mode_has_signal_db(&startup_mode) {
        let settings_dir = Path::new(&settings.args.config_file_path)
            .parent()
            .unwrap_or(Path::new("."))
            .to_string_lossy()
            .to_string();

        let signal_db = desk_signal::db::init_db(&settings_dir).await?;
        // Age-based retention cleanup for the local usage rollups (collect-only
        // telemetry, no billing coupling). One task per process; the delete is
        // idempotent.
        tokio::spawn(desk_signal::usage_retention::run_retention_cleanup_loop(
            signal_db.clone(),
        ));
    }

    info!("Server settings: {:?}", settings);
    // Get server execution file path
    let exec_file_path = env::current_exe()?;
    info!("Server execution file path: {:?}", exec_file_path);

    // Create a path to the static files directory, which is assumed to be in the same directory as the executable.
    let mut static_file_path = exec_file_path.clone();
    static_file_path.pop();
    static_file_path.push("static");
    info!("Server static file path: {:?}", static_file_path);
    let secret_key = Key::generate();
    let shared_settings_data = web::Data::from(shared_settings.clone());
    let settings_coordinator_data = web::Data::from(settings_coordinator.clone());

    // The TauriLoginToken is shared between the HTTP `/login_tauri` route (which
    // verifies + consumes) and the host-control `/ws/tauri_ipc` endpoint (which
    // refreshes the token on every Tauri reconnect). Cloning the struct shares
    // the inner `Arc<Mutex<>>`. We always start empty — the hub endpoint pushes
    // a fresh token via the ws Ready first frame on every Tauri reconnect.
    let shared_tauri_login_token: TauriLoginToken = TauriLoginToken::empty();
    let tauri_login_token: web::Data<Option<TauriLoginToken>> =
        web::Data::new(Some(shared_tauri_login_token.clone()));

    // Build host-control endpoint state once, so portable + daemon can mount
    // identical routes. The state is registered as actix Data inside
    // `register_routes` for both the inline portable App below and any
    // future caller of `configure_api_routes`.
    let host_control_endpoint_state: Option<Arc<host_control::endpoint::EndpointState>> =
        if let Some(hub) = host_control_hub.clone() {
            let ipc_token = shared_settings_data
                .read()
                .await
                .system
                .tauri_ipc_token
                .clone()
                .unwrap_or_default();
            if ipc_token.is_empty() {
                warn!("tauri_ipc_token is empty; /ws/tauri_ipc will reject all connections");
            }
            Some(Arc::new(
                host_control::endpoint::EndpointState::new(
                    hub,
                    ipc_token,
                    shared_tauri_login_token.clone(),
                )
                .with_settings(shared_settings.clone().into())
                .with_settings_coordinator(settings_coordinator.clone()),
            ))
        } else {
            None
        };

    let connection_map = web::Data::new(SharedConnectionMap::from(BTreeMap::new()));

    // The live connection→device binding map, shared by the signaling handlers
    // (which record it) and the TURN usage collector (which resolves it). The
    // portable server is single-process, so it is purely node-local.
    let conn_device_map: web::Data<desk_signal::turn_usage::ConnectionDeviceMap> =
        web::Data::new(desk_signal::turn_usage::ConnectionDeviceMap::default());

    // The TURN runtime is supervised rather than started once: settings can
    // change while the server runs, and every reader (ICE issuance, the info
    // endpoint, usage accounting) resolves the runtime through the supervisor
    // instead of holding whatever existed at startup.
    let turn_settings = {
        let settings = shared_settings.read().await;
        settings.turn.clone()
    };
    let initial_turn_revision = 1;
    let startup_plan = turn_plan(&startup_mode, &turn_settings, initial_turn_revision);
    match startup_plan.posture.intent {
        TurnIntent::Run => info!("Starting turn server"),
        // Deliberate, so not an error — but worth saying out loud, because a
        // host with no relay of its own falls back to whatever the signaling
        // server offers and traverses fewer NATs.
        TurnIntent::Disabled => {
            warn!("TURN service is disabled in settings; this host will not relay")
        }
        TurnIntent::NotConfigured => {
            warn!(
                "TURN service is enabled but no interface is configured; this host will not relay"
            )
        }
        TurnIntent::Unsupported => {}
    }
    // Say which configured entries are unusable, whatever the intent: a host
    // whose interfaces were all rejected reports "not configured", and without
    // this the operator would be told nothing about the entries they wrote.
    for rejected in &startup_plan.posture.rejected_interfaces {
        error!(
            "TURN interface #{} is not served: {}",
            rejected.index, rejected.detail
        );
    }
    let (turn_posture_tx, turn_posture_rx) =
        tokio::sync::watch::channel(startup_plan.posture.clone());
    let turn_supervisor = desk_turn::supervisor::spawn(
        Arc::new(HostTurnDriver::new(connection_map.clone())),
        startup_plan.desired,
        desk_turn::supervisor::BackoffConfig::default(),
    );
    let turn_runtime_view = web::Data::new(TurnRuntimeView::new(
        turn_supervisor.clone(),
        turn_posture_rx,
    ));
    let turn_control = web::Data::new(TurnRuntimeControl::new(
        startup_mode.clone(),
        turn_supervisor.clone(),
        turn_posture_tx,
        initial_turn_revision,
    ));
    // Held as app data so it is dropped exactly when the HTTP server stops.
    let turn_stop_guard = web::Data::new(TurnRuntimeStopGuard::new(turn_supervisor.clone()));

    if mode_hosts_turn(&startup_mode) {
        // Collect per-device TURN usage into the local sqlite rollup for as long
        // as the server runs, following the runtime across restarts. Only the
        // modes that can host TURN have both a runtime to account for and the
        // local DB the rollup lives in.
        let collector = crate::service::turn_usage_collector::TurnUsageCollector::new(
            crate::service::turn_usage_collector::StatisticsSource::Runtime(
                turn_supervisor.subscribe_runtime(),
            ),
            conn_device_map.clone().into_inner(),
        );
        tokio::spawn(collector.run());
    }

    // For Default / DeskServer modes that don't yet have a hub injected, fall back
    // to a Local hub so business code never sees a None. Approvals deny-fast when
    // no Tauri shell is connected (intended fallback for headless DeskServer).
    let host_control_hub_arc: Arc<host_control::HostControlHub> = match host_control_hub.clone() {
        Some(h) => h,
        None => Arc::new(host_control::HostControlHub::new_local()),
    };
    let host_control_hub_data: web::Data<Option<Arc<host_control::HostControlHub>>> =
        web::Data::new(Some(host_control_hub_arc.clone()));

    // Host→manager link status: shared between the signaling proxy (which records a
    // fatal device-quota rejection and parks until manual retry) and the REST API
    // (which surfaces the rejection and triggers a reconnect).
    let manager_link_state = Arc::new(daemon::manager_link_state::ManagerLinkState::new());
    let manager_link_state_data = web::Data::new(manager_link_state.clone());

    // On-demand temporary-support lifecycle: shared between the signaling proxy
    // (drives the support upstream) and the REST API (start / stop / status).
    let support_link_state = Arc::new(daemon::support_link_state::SupportLinkState::new());
    let support_link_state_data = web::Data::new(support_link_state.clone());

    // Shared "should the manager link be connected" gate: driven by the settings
    // controllers (host toggling the manager connection) and observed by the
    // signaling proxy (tears the current manager / support upstream down on
    // disable) and the fleet audit sink (stays purely local when disabled). Its
    // initial value is derived from the persisted settings.
    let manager_link_gate = {
        let s = shared_settings_data.read().await;
        Arc::new(daemon::manager_link_gate::ManagerLinkGate::new(
            daemon::signaling_proxy::manager_link_should_connect(
                &s.system.manager_url,
                &s.system.manager_api_token,
                s.system.manager_enabled,
            ),
        ))
    };
    let manager_link_gate_data = web::Data::new(manager_link_gate.clone());

    // If this instance runs signaling, ensure local_signaling_token is generated and persisted
    if startup_mode == StartupMode::Default || startup_mode == StartupMode::Signaling {
        let mut s = shared_settings_data.write().await;
        if s.system.local_signaling_token.is_none() {
            let token = uuid::Uuid::new_v4().to_string();
            // Never log the token value: it is a host signaling credential.
            info!("Generated new local_signaling_token");
            s.system.local_signaling_token = Some(token);
            if let Err(e) = s.save() {
                error!("Failed to save local_signaling_token: {}", e);
            }
        }
    }

    let validator: Arc<dyn desk_signal_facade::service::NodeTokenValidator> =
        Arc::new(crate::service::signaling::LocalNodeTokenValidator {
            settings: shared_settings_data.clone(),
        });
    let validator_data = web::Data::new(validator);

    // Start the desk pipeline.
    //
    // Both **Default (portable)** and **DeskServer (headless)** route
    // through the in-process daemon-worker pipeline so the WebRTC
    // PeerConnection lives in the daemon-side code path identical to
    // ServiceDaemon mode. The signaling proxy's `local_handle` skips
    // itself in DeskServer mode (only Default + ServiceDaemon expose a
    // local signaling endpoint), so DeskServer naturally runs only the
    // remote signaling + remote manager WS clients — matching the
    // headless "connect to remote signal server" role.
    match startup_mode {
        StartupMode::Default | StartupMode::DeskServer => {
            info!("Starting desk pipeline (in-process daemon, mode={startup_mode:?})");
            let settings_clone = shared_settings_data.clone();
            let session_hub = host_control_hub_arc.clone();
            let args_clone = settings.args.clone();
            let proxy_link_state = manager_link_state.clone();
            let proxy_support_state = support_link_state.clone();
            let proxy_link_gate = manager_link_gate.clone();
            let daemon_coordinator = settings_coordinator.clone();
            // This node's own bundled-TURN endpoints, read from the same live
            // runtime the local signaling issues TURN credentials from, so the
            // daemon's PC manager never relays through itself — including after
            // a settings change moves or stops the relay.
            let own_turn_endpoints = daemon::pc_manager::OwnTurnEndpoints::from_runtime(
                turn_supervisor.subscribe_runtime(),
            );
            actix_web::rt::spawn(async move {
                if let Err(e) = daemon::start_inprocess_daemon(
                    args_clone,
                    settings_clone,
                    daemon_coordinator,
                    session_hub,
                    own_turn_endpoints,
                    proxy_link_state,
                    proxy_support_state,
                    proxy_link_gate,
                )
                .await
                {
                    error!("In-process daemon failed to start: {e}");
                }
            });
        }
        _ => {}
    }

    // Start the Actix web server
    let mut http_server = HttpServer::new(move || {
        let default_static_file_path = static_file_path.clone();

        let turn_runtime_view = turn_runtime_view.clone();
        let turn_control = turn_control.clone();
        let turn_stop_guard = turn_stop_guard.clone();
        let startup_mode = startup_mode.clone();
        let tauri_login_token = tauri_login_token.clone();
        let host_control_hub_data = host_control_hub_data.clone();
        let validator_data = validator_data.clone();
        let manager_link_state_data = manager_link_state_data.clone();
        let support_link_state_data = support_link_state_data.clone();
        let manager_link_gate_data = manager_link_gate_data.clone();
        let host_control_endpoint_state = host_control_endpoint_state.clone();
        let surface_opts = ApiSurfaceOpts {
            include_signaling: matches!(
                startup_mode,
                StartupMode::Default | StartupMode::Signaling
            ),
            include_device_code: matches!(
                startup_mode,
                StartupMode::Default | StartupMode::Signaling
            ),
            has_signal_db: startup_mode_has_signal_db(&startup_mode),
            include_model_usage: matches!(
                startup_mode,
                StartupMode::Default | StartupMode::Signaling
            ),
        };
        App::new()
            .into_utoipa_app()
            .map(|app| app.wrap(Logger::default()))
            .map(|app| {
                app.wrap_fn(|req, srv| {
                    let method = req.method().clone();
                    let uri = req.uri().to_string();
                    let peer = req.connection_info().realip_remote_addr().map(str::to_owned);
                    let fut = srv.call(req);

                    async move {
                        let res = fut.await;
                        match &res {
                            Err(err) => {
                                error!(
                                    "HTTP request failed: method={}, uri={}, peer={:?}, error={}, debug={:?}",
                                    method, uri, peer, err, err
                                );
                            }
                            Ok(resp) if resp.status().is_server_error() => {
                                error!(
                                    "HTTP request returned server error: method={}, uri={}, peer={:?}, status={}",
                                    method,
                                    uri,
                                    peer,
                                    resp.status()
                                );
                            }
                            _ => {}
                        }
                        res
                    }
                })
            })
            .app_data(shared_settings_data.clone())
            .app_data(settings_coordinator_data.clone())
            .app_data(tauri_login_token.clone())
            .app_data(connection_map.clone())
            .app_data(conn_device_map.clone())
            .app_data(host_control_hub_data.clone())
            .app_data(validator_data.clone())
            .app_data(manager_link_state_data.clone())
            .app_data(support_link_state_data.clone())
            .app_data(manager_link_gate_data.clone())
            .app_data(turn_runtime_view.clone())
            .app_data(turn_control.clone())
            .app_data(turn_stop_guard.clone())
            .app_data(api_json_config()) // limit payload size + uniform error body
            .configure(|cfg| {
                if let Some(state) = host_control_endpoint_state.clone() {
                    log::info!(
                        "Registering host control routes (mode={:?})",
                        state.hub.mode()
                    );
                    // Host-control `/ws/*` are plain actix (non-utoipa) and
                    // depend on runtime endpoint state, so they stay with the
                    // caller. Bridge into the inner plain `ServiceConfig` via
                    // `.map` to reuse `register_routes` (whose closure must
                    // return `inner`).
                    cfg.map(|inner| {
                        host_control::endpoint::register_routes(inner, state);
                        inner
                    });
                }
            })
            .configure(move |cfg| configure_api_surface(cfg, surface_opts))
            // The runtime OpenAPI / doc-UI endpoints (swagger-ui / redoc / rapidoc
            // / scalar / openapi.json) are deliberately NOT served: the typed
            // frontend client is generated offline via `dump-openapi`
            // ([`build_openapi`]), so serving them at runtime would only add an
            // unauthenticated public attack surface to a self-hosted server.
            .into_app()
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), secret_key.clone())
                    .cookie_secure(cookie_secure_from_env())
                    // Encrypt (not just sign) the session cookie: the code-session
                    // principal it carries must be neither readable nor forgeable by
                    // the client. HttpOnly keeps it out of page JavaScript.
                    .cookie_content_security(actix_session::config::CookieContentSecurity::Private)
                    .cookie_http_only(true)
                    .build(),
            )
            .service(
                actix_files::Files::new("/", static_file_path.clone())
                    .index_file("index.html")
                    .default_handler(fn_service(move |req: ServiceRequest| {
                        // support html5 history mode
                        let (http_req, _payload) = req.into_parts();
                        let path = default_static_file_path.clone().join("index.html");
                        log::debug!(
                            "Default handler hit for path: {}, serving index.html",
                            http_req.path()
                        );
                        async {
                            let response =
                                actix_files::NamedFile::open(path)?.into_response(&http_req);
                            Ok(ServiceResponse::new(http_req, response))
                        }
                    })),
            )
    });
    let ipv6_active = settings.system.enable_ipv6 && check_ipv6_available();
    for addr in resolve_bind_addrs(
        ipv6_active,
        settings.system.listen_addr_ipv4.as_str(),
        settings.system.listen_addr_ipv6.as_str(),
        settings.system.port,
        cfg!(windows),
    ) {
        http_server = http_server.bind(addr.as_str())?;
        info!("Server started at http://{}", addr);
    }
    if embedded_should_disable_signals(host_control_hub.is_some()) {
        // The Tauri shell owns the process lifecycle. Letting actix trap SIGINT
        // would stop only the HTTP server while the GUI keeps running, so the
        // process never exits on Ctrl+C — see `embedded_should_disable_signals`.
        http_server = http_server.disable_signals();
    }
    let server = http_server.run();
    Ok((server, telemetry_guard))
}

/// Whether the embedded HTTP server should suppress actix's default
/// SIGINT/SIGTERM handling.
///
/// True only when a shared hub is supplied, i.e. the server is embedded in the
/// Tauri shell, which owns the process lifecycle: if actix traps SIGINT there it
/// gracefully stops only the HTTP server while the GUI event loop and the IPC
/// reconnect loop keep running, so Ctrl+C never terminates the app. Standalone /
/// headless runs (no shared hub) keep actix's graceful shutdown.
fn embedded_should_disable_signals(has_shared_hub: bool) -> bool {
    has_shared_hub
}

/// Compute the socket addresses the HTTP server should bind.
///
/// On Windows an IPv6 wildcard socket is `v6only` by default, so binding `::`
/// alone does not accept IPv4 clients (browsers on `127.0.0.1`, peers reaching
/// the server over an IPv4 LAN address). When IPv6 is active we therefore
/// additionally bind the IPv4 wildcard on Windows. On other platforms `::` is
/// dual-stack and already accepts IPv4-mapped clients, so binding the IPv4
/// wildcard too would collide on the same port.
fn resolve_bind_addrs(
    ipv6_active: bool,
    ipv4_addr: &str,
    ipv6_addr: &str,
    port: u16,
    is_windows: bool,
) -> Vec<String> {
    if ipv6_active {
        let mut addrs = vec![format!("{ipv6_addr}:{port}")];
        if is_windows {
            addrs.push(format!("{ipv4_addr}:{port}"));
        }
        addrs
    } else {
        vec![format!("{ipv4_addr}:{port}")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_secure_defaults_off_and_parses_truthy() {
        // Default (unset / blank / unrecognized) is false so local HTTP keeps its
        // session cookie.
        assert!(!parse_cookie_secure(None));
        assert!(!parse_cookie_secure(Some("")));
        assert!(!parse_cookie_secure(Some("nope")));
        assert!(!parse_cookie_secure(Some("0")));
        // Truthy values opt into Secure for an HTTPS deployment.
        assert!(parse_cookie_secure(Some("1")));
        assert!(parse_cookie_secure(Some("true")));
        assert!(parse_cookie_secure(Some("TRUE")));
        assert!(parse_cookie_secure(Some("yes")));
    }

    #[test]
    fn windows_with_ipv6_binds_both_stacks() {
        let addrs = resolve_bind_addrs(true, "0.0.0.0", "::", 8081, true);
        assert_eq!(
            addrs,
            vec![":::8081".to_string(), "0.0.0.0:8081".to_string()]
        );
    }

    #[test]
    fn non_windows_with_ipv6_binds_ipv6_only() {
        let addrs = resolve_bind_addrs(true, "0.0.0.0", "::", 8081, false);
        assert_eq!(addrs, vec![":::8081".to_string()]);
    }

    #[test]
    fn ipv6_inactive_binds_ipv4_only() {
        let addrs = resolve_bind_addrs(false, "0.0.0.0", "::", 8081, true);
        assert_eq!(addrs, vec!["0.0.0.0:8081".to_string()]);
    }

    // Constant-time eq is correct on equal slices.
    #[test]
    fn constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    // Embedded (Tauri) runs pass a shared hub and must suppress actix signal
    // handling so Ctrl+C terminates the whole process.
    #[test]
    fn embedded_disables_signals_when_hub_present() {
        assert!(embedded_should_disable_signals(true));
    }

    // Standalone / headless runs keep actix's graceful SIGINT shutdown.
    #[test]
    fn standalone_keeps_signals_without_hub() {
        assert!(!embedded_should_disable_signals(false));
    }

    #[test]
    fn constant_time_eq_unequal() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hellos")); // length mismatch
    }

    // TauriLoginToken::empty constructs in the unset state — verify always
    // fails until a refresh sets a token.
    #[test]
    fn tauri_login_token_empty_never_validates() {
        let token = TauriLoginToken::empty();
        assert!(!token.verify_and_consume(""));
        assert!(!token.verify_and_consume("anything"));
    }

    // After refresh, the empty token validates exactly once with the new value.
    #[test]
    fn tauri_login_token_empty_refresh_then_consume() {
        let token = TauriLoginToken::empty();
        token.refresh("new-tok".to_string());
        assert!(!token.verify_and_consume("wrong"));
        assert!(token.verify_and_consume("new-tok"));
        // Already consumed.
        assert!(!token.verify_and_consume("new-tok"));
    }

    // The shared inner Arc<Mutex> means cloning the struct gives a view onto
    // the same state — required for the HTTP route + ws endpoint to stay in sync.
    #[test]
    fn tauri_login_token_clone_shares_state() {
        let a = TauriLoginToken::empty();
        let b = a.clone();
        a.refresh("via-a".to_string());
        assert!(b.verify_and_consume("via-a"));
        // After consuming via b, a sees the consumed (empty) state too.
        assert!(!a.verify_and_consume("via-a"));
    }

    // Lock down the `ServerHandle` contract: the second tuple slot MUST be
    // `Option<WorkerGuard>` so callers can keep the `tracing_appender`
    // non-blocking writer alive for the entire `server.await` lifetime. The
    // earlier shape was `Result<Server, _>`, which let `run_with_hub` drop
    // its locally-bound guard at function return — closing the writer thread
    // and silently discarding every subsequent log line emitted by the
    // running server. Regressing the type (e.g. removing the guard slot or
    // returning a bare `Server`) breaks this assignment at compile time.
    #[test]
    fn server_handle_alias_carries_telemetry_worker_guard() {
        fn _shape_check(handle: ServerHandle) -> (Server, Option<WorkerGuard>) {
            handle
        }
        let _coerce: fn(ServerHandle) -> (Server, Option<WorkerGuard>) = _shape_check;
    }

    /// Build the daemon-style HTTP App (utoipa surface with daemon opts +
    /// session middleware) and assert each probe matches a registered route
    /// (status != 404). A middleware/handler rejection (e.g. 401 without a
    /// session) still proves the route is registered — same success criterion
    /// as before the single-source-of-truth refactor.
    async fn assert_daemon_routes_registered(probes: &[(&str, &str)]) {
        use crate::model::settings::{Settings, SharedSettings};
        use actix_web::test;
        use desk_signal::model::SharedConnectionMap;
        use utoipa_actix_web::AppExt as _;

        let settings: web::Data<SharedSettings> =
            web::Data::from(Arc::new(SharedSettings::from(Settings::default())));
        let secret_key = Key::generate();
        let app = test::init_service(
            App::new()
                .into_utoipa_app()
                .app_data(settings)
                .app_data(web::Data::new(None::<TauriLoginToken>))
                .app_data(web::Data::new(SharedConnectionMap::from(BTreeMap::new())))
                .app_data(web::Data::new(None::<Arc<host_control::HostControlHub>>))
                .configure(|cfg| {
                    configure_api_surface(
                        cfg,
                        ApiSurfaceOpts {
                            include_signaling: true,
                            include_device_code: true,
                            has_signal_db: true,
                            include_model_usage: true,
                        },
                    )
                })
                .into_app()
                .wrap(
                    SessionMiddleware::builder(CookieSessionStore::default(), secret_key)
                        .cookie_secure(cookie_secure_from_env())
                        // Encrypt (not just sign) the session cookie: the
                        // code-session principal it carries must be neither readable
                        // nor forgeable by the client. HttpOnly keeps it out of page
                        // JavaScript.
                        .cookie_content_security(
                            actix_session::config::CookieContentSecurity::Private,
                        )
                        .cookie_http_only(true)
                        .build(),
                ),
        )
        .await;

        for &(method, uri) in probes {
            let req = match method {
                "GET" => test::TestRequest::get().uri(uri).to_request(),
                "POST" => test::TestRequest::post().uri(uri).to_request(),
                "PUT" => test::TestRequest::put().uri(uri).to_request(),
                "DELETE" => test::TestRequest::delete().uri(uri).to_request(),
                _ => unreachable!("unhandled method {method}"),
            };
            match test::try_call_service(&app, req).await {
                Ok(resp) => assert_ne!(
                    resp.status(),
                    actix_web::http::StatusCode::NOT_FOUND,
                    "{method} {uri} returned 404 — route must be registered by \
                     configure_api_surface (daemon opts)",
                ),
                // A middleware/handler rejection (e.g. 401) means the route matched.
                Err(_) => {}
            }
        }
    }

    /// Regression: remote terminal endpoints must be exposed on the daemon surface.
    #[actix_web::test]
    async fn daemon_surface_registers_terminal_endpoints() {
        assert_daemon_routes_registered(&[
            ("GET", "/api/desk/terminals/test"),
            ("GET", "/api/desk/terminal/test?command=cmd"),
        ])
        .await;
    }

    /// Regression: virtual-display endpoints must be exposed on the daemon surface.
    #[actix_web::test]
    async fn daemon_surface_registers_virtual_display_endpoints() {
        assert_daemon_routes_registered(&[
            ("GET", "/api/virtual-display/driver/status"),
            ("POST", "/api/virtual-display/driver/install"),
            ("POST", "/api/virtual-display/driver/uninstall"),
            ("GET", "/api/desk/settings/virtual-display"),
            ("POST", "/api/desk/settings/virtual-display"),
        ])
        .await;
    }

    /// Regression: AI execution policy endpoint exposed at its real nested path
    /// (`/api` → `/desk` → `/settings/ai-policy`).
    #[actix_web::test]
    async fn daemon_surface_registers_ai_policy_settings_endpoint() {
        assert_daemon_routes_registered(&[
            ("GET", "/api/desk/settings/ai-policy"),
            ("POST", "/api/desk/settings/ai-policy"),
        ])
        .await;
    }

    /// Regression: device-code admin CRUD exposed on the daemon surface.
    #[actix_web::test]
    async fn daemon_surface_registers_device_code_endpoints() {
        assert_daemon_routes_registered(&[
            ("GET", "/api/desk/device_codes"),
            ("POST", "/api/desk/device_codes"),
            ("PUT", "/api/desk/device_codes/1"),
            ("DELETE", "/api/desk/device_codes/1"),
            ("POST", "/api/desk/device_codes/batch_delete"),
        ])
        .await;
    }

    /// The registered paths for a given surface, asserted at the spec level: an
    /// HTTP probe cannot distinguish "route absent" from the `/api` scope's
    /// anonymous-rejection 401.
    fn registered_paths(opts: ApiSurfaceOpts) -> Vec<String> {
        use utoipa_actix_web::AppExt as _;

        let (_app, api) = App::new()
            .into_utoipa_app()
            .configure(|cfg| configure_api_surface(cfg, opts))
            .split_for_parts();
        api.paths.paths.keys().cloned().collect()
    }

    /// TURN *runtime management* is registered on every surface, including the
    /// daemon's — whether a relay is running changes while the process runs, so
    /// a route table fixed at App construction cannot express it. The endpoints
    /// answer "not running, here is why"; only the usage history, which reads
    /// the signal DB, is gated.
    #[test]
    fn daemon_opts_still_register_the_turn_runtime_endpoints() {
        let mut turn_paths: Vec<String> = registered_paths(ApiSurfaceOpts {
            include_signaling: true,
            include_device_code: true,
            has_signal_db: true,
            include_model_usage: true,
        })
        .into_iter()
        .filter(|p| p.starts_with("/api/turn"))
        .collect();
        turn_paths.sort();

        assert_eq!(
            turn_paths,
            vec![
                "/api/turn/info",
                "/api/turn/metrics",
                "/api/turn/session",
                "/api/turn/session/statistics",
                "/api/turn/usage",
            ],
            "the daemon surface must expose the runtime endpoints as well as the \
             usage history; got {turn_paths:?}",
        );
    }

    /// `/api/turn/usage` only reads the local rollup tables, so its availability
    /// follows the signal DB and never the TURN runtime. Both directions matter:
    /// a server with no TURN must still show its history (otherwise the page
    /// 404s), and a process without the DB must not register it at all
    /// (`desk_signal::db::get_db()` panics when the DB was never opened).
    #[test]
    fn turn_usage_follows_the_signal_db_not_the_turn_runtime() {
        // (label, has_signal_db, expect_usage)
        let surfaces = [
            ("Default", true, true),
            ("Signaling", true, true),
            ("ServiceDaemon local API", true, true),
            ("DeskServer", false, false),
        ];

        for (label, has_signal_db, expect_usage) in surfaces {
            let paths = registered_paths(ApiSurfaceOpts {
                include_signaling: has_signal_db,
                include_device_code: has_signal_db,
                has_signal_db,
                include_model_usage: has_signal_db,
            });

            assert_eq!(
                paths.iter().any(|p| p == "/api/turn/usage"),
                expect_usage,
                "{label}: /api/turn/usage registration",
            );
            assert!(
                paths.iter().any(|p| p == "/api/turn/info"),
                "{label}: the runtime status endpoint must exist everywhere, so a \
                 host with no relay can say so instead of 404ing",
            );
        }
    }

    /// The route gate and the DB bootstrap read the same table, so a mode can
    /// never serve a `get_db()`-backed handler without having opened the DB.
    #[test]
    fn signal_db_modes_are_enumerated_exhaustively() {
        for mode in [
            StartupMode::Default,
            StartupMode::Signaling,
            StartupMode::ServiceDaemon,
        ] {
            assert!(
                startup_mode_has_signal_db(&mode),
                "{mode:?} opens the signal DB",
            );
        }
        for mode in [
            StartupMode::DeskServer,
            StartupMode::SessionWorker,
            StartupMode::McpStdio,
        ] {
            assert!(
                !startup_mode_has_signal_db(&mode),
                "{mode:?} never opens the signal DB",
            );
        }
    }

    /// The offline spec is a superset built through the same scope nesting as the
    /// live servers, so every conditional route appears with its real `/api`
    /// prefix (not the bare path the old `AllPathsDoc` would emit).
    #[test]
    fn build_openapi_includes_prefixed_conditional_routes() {
        let api = build_openapi();
        for expected in [
            "/api/desk/settings",
            "/api/desk/device_codes",
            "/api/desk/signaling",
            "/api/my/diagnose-session",
            "/api/my/diagnose-sessions",
            "/api/turn/info",
        ] {
            assert!(
                api.paths.paths.contains_key(expected),
                "build_openapi() missing {expected}; got {:?}",
                api.paths.paths.keys().collect::<Vec<_>>(),
            );
        }
    }

    /// `DeskErrorCode` is referenced by no request or response body, so route
    /// collection alone would drop it. It reaches the spec only through the
    /// explicit `ExtraSchemas` registration, and it is what lets the generated
    /// client expose named constants instead of the numbers being mirrored by
    /// hand — if the component disappears, every consumer silently falls back to
    /// bare integers.
    #[test]
    fn build_openapi_publishes_the_error_code_enum() {
        use desk_utils::error::DeskErrorCode;

        let api = build_openapi();
        let schema = api
            .components
            .as_ref()
            .expect("components")
            .schemas
            .get("DeskErrorCode")
            .expect("DeskErrorCode component");
        let schema = serde_json::to_value(schema).expect("schema serializes");

        assert_eq!(schema["type"], "integer");
        assert_eq!(schema["format"], "int32");

        // The generator pairs the two arrays by index, so their order is part of
        // the contract, not an implementation detail.
        let values = schema["enum"].as_array().expect("enum array");
        let names = schema["x-enum-varnames"]
            .as_array()
            .expect("x-enum-varnames array");
        assert_eq!(values.len(), DeskErrorCode::ALL.len());
        assert_eq!(names.len(), DeskErrorCode::ALL.len());
        for (index, (name, code)) in DeskErrorCode::ALL.iter().enumerate() {
            assert_eq!(names[index], *name);
            assert_eq!(values[index], *code);
        }
    }

    #[test]
    fn signaling_openapi_parameters_are_queries() {
        use utoipa::openapi::path::ParameterIn;

        let api = build_openapi();
        let operation = api.paths.paths["/api/desk/signaling"]
            .get
            .as_ref()
            .expect("signaling GET operation");
        let parameters = operation.parameters.as_ref().expect("signaling parameters");

        assert!(!parameters.is_empty());
        assert!(
            parameters
                .iter()
                .all(|parameter| parameter.parameter_in == ParameterIn::Query)
        );
    }

    /// Auth-order guard: signaling is registered at the top level, *before* the
    /// `/api` scope's `enforce_device_scope`. With a valid node token (and no
    /// session) the handler must pass its own auth and reach the WebSocket
    /// handshake — so the response is neither a `404` (route absent) nor a `401`
    /// (which would mean the anonymous-rejection middleware ran first because
    /// signaling was wrongly nested under `/api`).
    #[actix_web::test]
    async fn signaling_bypasses_anonymous_rejection_on_daemon_surface() {
        use crate::model::settings::{Settings, SharedSettings};
        use actix_web::test;
        use desk_signal::model::SharedConnectionMap;
        use desk_signal_facade::model::version::VersionInfo;
        use desk_signal_facade::model::{os::OperationSystemEnum, signal::RemoteDeskTypeEnum};
        use desk_signal_facade::service::NodeTokenValidator;
        use utoipa_actix_web::AppExt as _;

        struct AlwaysValidValidator;
        impl NodeTokenValidator for AlwaysValidValidator {
            fn validate_node_token<'a>(
                &'a self,
                _token: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>
            {
                Box::pin(async { true })
            }
        }

        let settings: web::Data<SharedSettings> =
            web::Data::from(Arc::new(SharedSettings::from(Settings::default())));
        let validator: web::Data<Arc<dyn NodeTokenValidator>> =
            web::Data::new(Arc::new(AlwaysValidValidator) as Arc<dyn NodeTokenValidator>);
        let secret_key = Key::generate();
        let app = test::init_service(
            App::new()
                .into_utoipa_app()
                .app_data(settings)
                .app_data(validator)
                .app_data(web::Data::new(None::<TauriLoginToken>))
                .app_data(web::Data::new(SharedConnectionMap::from(BTreeMap::new())))
                .app_data(web::Data::new(None::<Arc<host_control::HostControlHub>>))
                .configure(|cfg| {
                    configure_api_surface(
                        cfg,
                        ApiSurfaceOpts {
                            include_signaling: true,
                            include_device_code: true,
                            has_signal_db: true,
                            include_model_usage: true,
                        },
                    )
                })
                .into_app()
                .wrap(
                    SessionMiddleware::builder(CookieSessionStore::default(), secret_key)
                        .cookie_secure(cookie_secure_from_env())
                        // Encrypt (not just sign) the session cookie: the
                        // code-session principal it carries must be neither readable
                        // nor forgeable by the client. HttpOnly keeps it out of page
                        // JavaScript.
                        .cookie_content_security(
                            actix_session::config::CookieContentSecurity::Private,
                        )
                        .cookie_http_only(true)
                        .build(),
                ),
        )
        .await;

        let version = VersionInfo {
            api_version: 1,
            build_number: 1,
            commit_hash: "test".into(),
            remote_desk_type: RemoteDeskTypeEnum::Browser,
            operation_system: OperationSystemEnum::Windows,
            display_name: None,
            client_id: None,
            token: Some("test-token".into()),
            debug_build: false,
            repository_url: None,
            available_exec_shells: None,
            max_ai_command_runtime_ms: None,
        };
        let query = serde_urlencoded::to_string(&version).unwrap();
        let uri = format!("/api/desk/signaling?{query}");
        let req = test::TestRequest::get().uri(&uri).to_request();
        let resp = test::call_service(&app, req).await;
        // 400 = the handler ran, passed token auth, and reached the WebSocket
        // handshake which fails on the missing upgrade headers. A 401 would mean
        // `enforce_device_scope` ran first (signaling wrongly nested under
        // `/api`); a 404 would mean the route is absent.
        assert_eq!(
            resp.status(),
            actix_web::http::StatusCode::BAD_REQUEST,
            "signaling should bypass enforce_device_scope and reach the WS handshake (got {})",
            resp.status(),
        );
    }
}
