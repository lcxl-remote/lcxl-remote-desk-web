use std::fmt::{self, Display, Formatter};

pub fn format_backtrace<E: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    backtrace: &std::backtrace::Backtrace,
    error: &E,
) -> fmt::Result {
    if matches!(
        backtrace.status(),
        std::backtrace::BacktraceStatus::Captured
    ) {
        write!(f, "{}\n{}", error, backtrace)
    } else {
        error.fmt(f)
    }
}

pub fn format_debug_backtrace<E: fmt::Debug>(
    f: &mut fmt::Formatter<'_>,
    backtrace: &std::backtrace::Backtrace,
    error: &E,
) -> fmt::Result {
    if matches!(
        backtrace.status(),
        std::backtrace::BacktraceStatus::Captured
    ) {
        write!(f, "{:?}\n{}", error, backtrace)
    } else {
        f.write_fmt(format_args!("{:?}", error))
    }
}

/// A business error code.
///
/// Serializes as the bare integer it wraps, so a wire field typed as this is
/// indistinguishable from one typed `i32` — the type only keeps the Rust side
/// from writing a literal. Deserialization accepts any integer rather than only
/// the declared ones: a peer running a newer build may send a code this one has
/// never heard of, and rejecting the whole frame over an unrecognized code
/// would lose the error it was reporting.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
#[serde(transparent)]
pub struct DeskErrorCode(i32);

/// Declare every `DeskErrorCode` in one place.
///
/// The macro emits two projections that must never drift apart: the associated
/// constants callers already use, and the `ALL` name/value table the OpenAPI
/// schema — and therefore the generated TypeScript client — is built from. A
/// constant written by hand outside this macro still compiles, but stays
/// invisible to the client, so every code belongs in the invocation below.
macro_rules! desk_error_codes {
    ($(
        $(#[$meta:meta])*
        $name:ident = $value:literal
    ),* $(,)?) => {
        impl DeskErrorCode {
            $(
                $(#[$meta])*
                pub const $name: DeskErrorCode = DeskErrorCode($value);
            )*

            /// Every declared code as `(name, value)`, in declaration order.
            ///
            /// The OpenAPI schema turns this into a parallel `enum` /
            /// `x-enum-varnames` pair and the client generator matches the two
            /// by index, so the projections must keep equal length and order.
            pub const ALL: &'static [(&'static str, i32)] = &[
                $((stringify!($name), $value),)*
            ];
        }
    };
}

desk_error_codes! {
    SUCCESS = 0,
    SYSTEM_ERROR = 1,
    INVALID_STATE = 2,
    NOT_IMPLEMENTED_YET = 3,
    PERMISSION_ERROR = 4,
    INVALID_PARAMS = 5,
    UNKNOWN_SIGNALING_TYPE = 6,
    /// The requested feature/backend is structurally unavailable in the
    /// current process or desktop context (e.g. Windows.Graphics.Capture
    /// under the SYSTEM token / Winlogon desktop, where RuntimeBroker is
    /// not running). Callers may transparently fall back to an
    /// alternative implementation instead of surfacing this as a hard
    /// error.
    FEATURE_UNAVAILABLE = 7,
    /// The request is structurally valid but rejected because a
    /// hard precondition is unmet (e.g. the caller asked the daemon
    /// to enable the virtual display but the IDD driver is not
    /// staged). Use this when the right resolution is "make the
    /// precondition true and retry," not "fix the request body."
    PRECONDITION_FAILED = 8,
    /// The host is running Wayland but the locally owned Portal session does not yet provide the capabilities required by this connection.
    WAYLAND_PORTAL_AUTHORIZATION_REQUIRED = 9,

    FILE_PATH_NOT_FOUND = 11,
    CLIENT_ID_NOT_FOUND = 12,
    /// Optimistic-concurrency conflict: a write supplied an `expected_revision`
    /// that no longer matches the current persisted revision (another writer or
    /// instance committed in between). The caller should re-read the current
    /// revision/value — returned in the response payload — and retry. This is a
    /// business-level outcome carried in the `RestResponse.code`, never an HTTP
    /// status code.
    REVISION_CONFLICT = 13,
    /// A fleet (multi-device) request resolved to zero diagnosable targets after
    /// applying the caller's policy visibility. Returned uniformly whether the
    /// selector matched nothing or every match was policy-invisible, so it leaks
    /// no information about devices the caller cannot see. Carried in
    /// `RestResponse.code`, never an HTTP status.
    NO_VISIBLE_TARGETS = 14,

    // ---- Fleet batch execution (write path) ----
    /// A batch approval no longer matches the previewed plan: the draft fingerprint
    /// set, `preview_generation`, or one of the bound revisions
    /// (policy / template / guardrail) drifted between preview and the approval /
    /// execution attempt. The whole batch is stale and must be re-previewed; never
    /// a partial silent drop. Carried in `RestResponse.code`, never an HTTP status.
    FLEET_APPROVAL_STALE = 15,
    /// A high-risk batch did not satisfy the guardrail (blast-radius cap exceeded,
    /// or the required two-person review was not met). Carried in
    /// `RestResponse.code`, never an HTTP status.
    FLEET_HIGH_RISK_BLOCKED = 16,
    /// A batch execution preview resolved to zero executable targets (every device
    /// was not-executable / blocked / denied), so no execution task is created.
    /// Carried in `RestResponse.code`, never an HTTP status.
    FLEET_NOT_EXECUTABLE = 17,
    /// An approve / execute action was attempted on a dry-run task, which has no
    /// execution path by construction. Carried in `RestResponse.code`, never an
    /// HTTP status.
    FLEET_DRY_RUN_NOT_APPROVABLE = 18,
    /// The approver lacks `shell.exec.confirmed` on at least one covered device, so
    /// the whole approval fails (the approved set must equal exactly the previewed
    /// draft set — never silently narrowed). Distinct from a stale approval.
    /// Carried in `RestResponse.code`, never an HTTP status.
    FLEET_APPROVAL_FORBIDDEN = 19,
    /// A device lookup in the owner-scoped personal API found no live device with
    /// the given id owned by the requesting user. Returned uniformly whether the
    /// device does not exist, was soft-deleted, or belongs to another owner, so a
    /// personal user cannot probe other owners' device ids. Carried in
    /// `RestResponse.code`, never an HTTP status.
    DEVICE_NOT_FOUND = 20,

    NOT_ALLOW_DELETE_FILE = 21,
    FILE_CHANGED = 22,

    // ---- Login / registration anti-abuse (auth hardening) ----
    /// Authentication failed. Returned uniformly for every credential-rejection
    /// cause — unknown username, wrong password, or an account that is not active
    /// — so the response leaks nothing about which accounts exist or their state.
    /// The login path equalizes its work (a dummy password verify on the failure
    /// branch) so timing does not distinguish these cases either. Carried in
    /// `RestResponse.code`, never an HTTP status.
    ILLEGAL_CREDENTIALS = 30,
    /// The target account or client IP is temporarily locked after too many
    /// failed login attempts. The lock has a TTL and (for the username dimension)
    /// can be cleared by an administrator. Carried in `RestResponse.code`, never
    /// an HTTP status.
    ACCOUNT_LOCKED = 31,
    /// A rate limit was exceeded (e.g. registration attempts per IP, or
    /// verification-email resends per address). The caller should slow down and
    /// retry later. Carried in `RestResponse.code`, never an HTTP status.
    TOO_MANY_ATTEMPTS = 32,
    /// A human-verification (CAPTCHA) challenge is now required before the request
    /// can proceed — typically after the login failure count crosses the soft
    /// threshold. The client should render the challenge and resubmit with a
    /// token. Carried in `RestResponse.code`, never an HTTP status.
    CAPTCHA_REQUIRED = 33,
    /// A supplied human-verification token was missing, malformed, or rejected by
    /// the verifier (including fail-closed when the verifier is unreachable).
    /// Carried in `RestResponse.code`, never an HTTP status.
    CAPTCHA_FAILED = 34,
    /// The account exists but its email address has not been verified, so the
    /// requested action is refused. It is only ever returned once the caller has
    /// proven possession of the account — a correct password on the login path, a
    /// completed OAuth binding, or an explicit verification / resend flow — so it
    /// cannot be used to enumerate account state: every credential rejection stays
    /// generic (`ILLEGAL_CREDENTIALS`). Carried in `RestResponse.code`, never an
    /// HTTP status.
    EMAIL_NOT_VERIFIED = 35,
    /// Registration was refused because the (canonicalized) email or username is
    /// already taken. Returned with generic wording so it cannot be used to probe
    /// which addresses are registered. Carried in `RestResponse.code`, never an
    /// HTTP status.
    EMAIL_ALREADY_REGISTERED = 36,
    /// The supplied password did not meet the configured strength policy (length,
    /// character classes, upper bound). Carried in `RestResponse.code`, never an
    /// HTTP status.
    WEAK_PASSWORD = 37,
    /// A single-use token (email verification or password reset) was invalid,
    /// already consumed, or expired. Returned uniformly for all three so it
    /// reveals nothing about token existence. Carried in `RestResponse.code`,
    /// never an HTTP status.
    INVALID_OR_EXPIRED_TOKEN = 38,

    /// A request was throttled by a per-subject quota (e.g. too many terminal
    /// copilot asks in the window). Carried in `RestResponse.code` / streamed in a
    /// terminal AI error event, never an HTTP status; the client backs off and
    /// retries.
    RATE_LIMITED = 39,

    // ---- Organization (multi-tenant) ----
    /// An organization-scoped request referenced an org the caller cannot access:
    /// the org does not exist, was soft-deleted, or the caller is not a member.
    /// Returned uniformly for all three so a non-member cannot probe which org ids
    /// exist. Carried in `RestResponse.code`, never an HTTP status.
    ORG_NOT_FOUND = 40,
    /// The caller is a member of the organization but lacks the in-org role
    /// required for the action (e.g. a plain member attempting an org-admin write,
    /// or demoting/removing the last remaining owner). Carried in
    /// `RestResponse.code`, never an HTTP status.
    ORG_PERMISSION_ERROR = 41,
    /// An org admin tried to invite a user id that does not reference an existing
    /// user. Carried in `RestResponse.code`, never an HTTP status.
    USER_NOT_FOUND = 42,
    /// An accept/decline referenced no pending invite for the caller in that org
    /// (never invited, already responded, or revoked). Also returned to an org
    /// admin revoking a non-existent invite. Carried in `RestResponse.code`.
    INVITE_NOT_FOUND = 43,
    /// An invite targets a user who is already a member of the organization.
    /// Carried in `RestResponse.code`, never an HTTP status.
    ALREADY_ORG_MEMBER = 44,
    /// An invite already exists and is pending for this `(org, user)` pair.
    /// Carried in `RestResponse.code`, never an HTTP status.
    INVITE_ALREADY_PENDING = 45,

    // ---- Per-user device registration quota ----
    /// A new device registration was refused because the owner already holds the
    /// maximum number of devices allowed by their effective plan (a stock cap, not
    /// a per-period allowance). Existing devices keep working — only first-claim of
    /// a new `client_id` is blocked. The owner must soft-delete an unused device to
    /// free a slot, then retry. The manager delivers this in a signaling
    /// `Error(-1)` frame's `error_code` during the registration handshake, and the
    /// personal devices REST surface also branches on it. It is a fatal
    /// registration outcome: the host stops auto-reconnecting and surfaces a
    /// cleanup prompt. Never an HTTP status.
    DEVICE_QUOTA_EXCEEDED = 46,
    /// A token-authenticated desk-server registration handshake arrived without a
    /// non-empty `client_id`. Such a connection would otherwise bypass the device
    /// quota entirely (it neither registers a device nor counts against the cap),
    /// so it is rejected outright. Distinct from `DEVICE_QUOTA_EXCEEDED` so the
    /// host can show "missing device identity" rather than "device limit reached."
    /// Delivered in a signaling `Error(-1)` frame; also a fatal registration
    /// outcome that stops auto-reconnect. Never an HTTP status.
    DEVICE_CLIENT_ID_REQUIRED = 47,

    /// A new API token could not be created because the user already holds the
    /// maximum number of non-expired tokens (enabled or disabled both occupy a
    /// slot; only deleting one frees capacity). The cap shares the device-quota
    /// default threshold. The console surfaces the message; an auto-creating host
    /// client (e.g. the mobile host) treats it as a stop signal and prompts the
    /// user to remove a token or supply an existing one. Carried in
    /// `RestResponse.code`, never an HTTP status.
    API_TOKEN_QUOTA_EXCEEDED = 48,

    /// A subscription plan could not be physically deleted because one or more
    /// subscription segments still reference it (deleting the row would orphan
    /// their immutable snapshots and break the audit trail). The admin should
    /// disable the plan (`enabled = false`) instead, which stops new
    /// subscriptions while preserving history. Carried in `RestResponse.code`,
    /// never an HTTP status.
    PLAN_IN_USE = 49,

    /// The terminal copilot is turned off for this deployment (the fleet-wide
    /// enable flag is unset), so a copilot ask is refused. The control end maps
    /// this code to a localized message; the backend never sends a localized
    /// string. Rides the agent-error wire, not an HTTP status.
    TERMINAL_COPILOT_DISABLED = 50,
    /// No AI model provider is configured on the manager (provider / model /
    /// base URL / API key unset), so an agentic ask cannot dial a model. The
    /// control end maps this code to a localized "configure a model" message.
    /// Rides the agent-error wire, not an HTTP status.
    AI_MODEL_NOT_CONFIGURED = 51,
    /// The caller explicitly requested a model that is not in its resolution
    /// subject's gated catalog (its own tier plus the platform tier when the
    /// platform-fallback switch is on) — an out-of-catalog / disabled / archived
    /// model id. The request is rejected fail-closed rather than silently
    /// downgraded to a default. Rides the agent-error wire, not an HTTP status.
    AI_MODEL_NOT_AUTHORIZED = 54,

    /// A priced subscription plan has no recurring-price row matching the
    /// account's currency (neither an org-scoped override nor the platform
    /// default), so the subscription cannot snapshot a fee. The admin must add a
    /// price in that currency (or switch the plan to `free`). Carried in
    /// `RestResponse.code`; self-healing subscribe paths log a warning and skip.
    /// Never an HTTP status.
    PLAN_NO_PRICE = 52,
    /// A plan price row cannot be deleted because it is the required platform
    /// price (in the account default currency) of an enabled or default priced
    /// plan; removing it would drop the plan's default subscription back to
    /// `PLAN_NO_PRICE`. The admin must first disable the plan, switch it to
    /// `free`, or add a replacement price. Carried in `RestResponse.code`, never
    /// an HTTP status.
    PLAN_PRICE_REQUIRED = 53,
    /// A billing account cannot be switched to `prepaid` settlement because it still
    /// carries outstanding payable `point_debt`. Prepaid accounts are never billed by
    /// settlement, so the residual debt would strand forever; the admin must settle or
    /// absorb it before switching. Carried in `RestResponse.code`, never an HTTP status.
    SETTLEMENT_DEBT_OUTSTANDING = 55,

    /// No billing account exists for the referenced subject. Carried in
    /// `RestResponse.code`, never an HTTP status.
    BILLING_ACCOUNT_NOT_FOUND = 56,

    /// The agentic terminal copilot exhausted its per-turn step budget before
    /// producing an answer (the loop's step circuit-breaker tripped). The control
    /// end maps this code to a localized "ran out of steps" message. Rides the
    /// agent-error wire, not an HTTP status.
    COPILOT_STEP_LIMIT_EXCEEDED = 57,
    /// The terminal copilot's response was truncated before it completed. The
    /// control end maps this code to a localized message. Rides the agent-error
    /// wire, not an HTTP status.
    COPILOT_RESPONSE_TRUNCATED = 58,
    /// The model violated the copilot response contract (unparseable / malformed
    /// tool or answer envelope). The control end maps this code to a localized
    /// message. Rides the agent-error wire, not an HTTP status.
    COPILOT_PROTOCOL_VIOLATION = 59,
    /// Another copilot turn is already in progress for this conversation, so the
    /// new ask is refused. The control end maps this code to a localized message.
    /// Rides the agent-error wire, not an HTTP status.
    COPILOT_TURN_BUSY = 60,
    /// The copilot conversation belongs to a different session subject than the
    /// caller (a stale or cross-session continuation). The control end maps this
    /// code to a localized message. Rides the agent-error wire, not an HTTP status.
    COPILOT_SUBJECT_MISMATCH = 61,
    /// The agent stopped a turn because the model requested the same tool more
    /// times than the per-turn repeat circuit breaker permits. The control end
    /// maps this code to a localized loop-prevention message. Rides the
    /// agent-error wire, not an HTTP status.
    AGENT_SAME_TOOL_REPEAT_LIMIT = 70,
    /// The model requested a shell that the target did not report as usable by
    /// the AI executor. The model receives the target's verified shell list and
    /// may retry with one of those values; the control end may localize this code
    /// if it surfaces the tool error.
    AI_EXEC_SHELL_UNSUPPORTED = 71,
    /// The account is in a self-deletion state (`email_pending` / `grace` /
    /// `deleting` / `deleted`) and the requested mutating action is refused while
    /// the deletion is pending. The user must cancel the deletion first. Carried
    /// in `RestResponse.code` (business error, HTTP stays 200).
    ACCOUNT_PENDING_DELETION = 62,
    /// The account cannot be deleted because it still owns one or more
    /// organizations; ownership must be transferred or the organizations disbanded
    /// first. Carried in `RestResponse.code` (business error, HTTP stays 200).
    ACCOUNT_STILL_ORG_OWNER = 63,
    /// The account cannot be deleted because it is a platform administrator.
    /// Deleting it would leave a tombstone that still counts as an administrator,
    /// so the server would report itself initialized with nobody able to sign in
    /// and no way to bootstrap again. The role must be handed to another account
    /// first. Carried in `RestResponse.code` (business error, HTTP stays 200).
    ACCOUNT_IS_PLATFORM_ADMIN = 72,

    // ---- External identity and OAuth security core ----
    /// The account already has an identity for the requested provider, or the
    /// immutable provider key is already owned by another identity.
    IDENTITY_ALREADY_LINKED = 73,
    /// Removing or disabling the requested authentication method would leave
    /// the account with no usable way to sign in.
    LAST_LOGIN_METHOD = 74,
    /// The requested password-authenticated operation cannot run because this
    /// externally provisioned account has not set a password.
    PASSWORD_NOT_SET = 75,
    /// The selected OAuth provider is disabled or not fully configured.
    OAUTH_PROVIDER_DISABLED = 76,
    /// The provider code exchange or identity fetch failed. Details remain
    /// server-side to avoid leaking provider responses or credentials.
    OAUTH_EXCHANGE_FAILED = 77,
    /// A sensitive operation requires a fresh, action-bound reauthentication
    /// proof.
    REAUTH_REQUIRED = 78,
    /// The OAuth transaction is absent, expired, already consumed, or does not
    /// match its provider, browser session, or configuration revision.
    OAUTH_TXN_INVALID = 79,
    /// The OAuth continuation is absent, expired, already completed, fenced by
    /// another attempt, or otherwise invalid for this operation.
    OAUTH_CONTINUATION_INVALID = 80,
    /// A manager API token can no longer become valid: it is missing, expired,
    /// owned by a missing account, or its owner is being/deleted. Hosts tear down
    /// every session admitted by that credential and park same-token reconnects.
    MANAGER_CREDENTIAL_REVOKED = 81,
    /// A manager API token is temporarily unusable because the token or owner is
    /// disabled, or account deletion is still cancellable. Hosts tear down the
    /// credential scope but periodically retry the same token.
    MANAGER_CREDENTIAL_SUSPENDED = 82,
    /// A free-plan platform AI request exceeded the manager's estimated context
    /// token ceiling before any provider request, billing hold, or AI admission
    /// lease was created. The user must shorten the conversation or evidence.
    /// Rides the agent-error / REST business-error wire, never an HTTP status.
    AI_CONTEXT_LIMIT_EXCEEDED = 83,
    /// The bounded global queue for free-plan platform AI calls is full. No
    /// provider request or billing hold was created; a newly initiated request
    /// may retry later and joins the queue at a new tail position. Rides the
    /// agent-error / REST business-error wire, never an HTTP status.
    AI_PLATFORM_BUSY = 84,
    /// A request contains one or more image inputs, but the selected model is
    /// explicitly declared text-only. The caller must choose/configure a visual
    /// model or retry without screenshots; the server never silently drops the
    /// image or switches models. Rides agent-error / REST business-error wires.
    AI_MODEL_IMAGE_INPUT_UNSUPPORTED = 85,
    /// The platform content policy rejected the input, output, action, or image.
    /// The response never carries the rejected material or provider rationale.
    AI_CONTENT_BLOCKED = 86,
    /// A required platform content-safety verdict could not be obtained or
    /// validated. When content safety is enabled, protected AI fails closed and
    /// the user may retry this typed infrastructure failure.
    AI_CONTENT_SAFETY_UNAVAILABLE = 87,
    /// The platform safety model cannot review an image that would otherwise be
    /// sent to the selected Agent model. Distinct from code 85: code 85 describes
    /// the user-selected main model, while this code describes the mandatory
    /// platform reviewer.
    AI_CONTENT_SAFETY_IMAGE_UNSUPPORTED = 88,

    // ---- Video media pipeline ----
    /// The selected encoder cannot accept the capture frame dimensions. The
    /// controller should choose one of the compatible encoders reported by the
    /// host, or change the display mode before retrying.
    VIDEO_ENCODER_DIMENSIONS_UNSUPPORTED = 89,
    /// The selected encoder's runtime prepare probe failed for the current
    /// capture dimensions. This is distinct from a declared size limit because
    /// the implementation exposes no stable maximum ahead of construction.
    VIDEO_ENCODER_PREPARE_FAILED = 90,
    /// A media retry could not reuse an earlier StartMedia payload, so a fresh
    /// SDP offer is required before the host can restart the pipeline.
    VIDEO_PIPELINE_RENEGOTIATION_REQUIRED = 91,
    /// A bounded media retry failed while restarting the cached pipeline.
    VIDEO_PIPELINE_RESTART_FAILED = 92,
    /// A running encoder returned three consecutive frame-encode failures.
    VIDEO_PIPELINE_RUNTIME_FAILED = 93,

    // ---- Wayland Portal host readiness ----
    /// A ScreenAndInput request completed without both keyboard and pointer
    /// permission. The local user must grant both or choose a non-Portal input
    /// mode before Portal input can become ready.
    WAYLAND_PORTAL_INPUT_PERMISSION_REQUIRED = 94,
    /// The local user cancelled an in-progress Portal authorization.
    WAYLAND_PORTAL_AUTHORIZATION_CANCELLED = 95,
    /// A previously ready Portal session was closed or revoked.
    WAYLAND_PORTAL_SESSION_CLOSED = 96,
    /// The Portal backend failed to create or validate a usable session. Raw
    /// backend details are diagnostic-only; UIs localize this code.
    WAYLAND_PORTAL_BACKEND_FAILED = 97,
    /// Automatic IDD resolution changes require exactly one active remote
    /// desktop peer. The preference remains stored, but this request cannot be
    /// applied while another desktop peer is present.
    ADAPTIVE_RESOLUTION_REQUIRES_SINGLE_CLIENT = 98,
    /// The current worker has not published the media capability snapshot needed
    /// to construct an executable remote-desktop session.
    REMOTE_DESKTOP_CAPABILITIES_NOT_READY = 99,
    /// An in-process worker could not stop its media threads within the bounded
    /// fail-safe window. The host process must be restarted before new media
    /// sessions can be admitted.
    MEDIA_WORKER_RESTART_REQUIRED = 100,

    /// A connection-verify probe could not reach the target at all (DNS failure,
    /// connection refused, TLS handshake failure). Carried inside the
    /// `ConnectionVerifyResult` for display.
    CONNECTION_UNREACHABLE = 64,
    /// A connection-verify probe reached an endpoint but it did not identify
    /// itself as a desk signaling endpoint (missing probe marker header), so it is
    /// not usable as a signaling / manager target.
    CONNECTION_NOT_SIGNALING = 65,
    /// A connection-verify probe reached the signaling endpoint but the API token
    /// was rejected (or absent).
    CONNECTION_AUTH_FAILED = 66,
    /// A connection-verify target was refused before dialing: an unsupported URL
    /// scheme, or an address blocked by the SSRF guard.
    CONNECTION_TARGET_BLOCKED = 67,
    /// A connection-verify target resolved to a public address dialed over a
    /// plaintext scheme (`ws://` / `http://`) while `require_secure_signaling` is
    /// on. Distinct from `CONNECTION_TARGET_BLOCKED` so the wizard can prompt the
    /// user to use TLS (`wss://`) or, deliberately, disable the switch — rather
    /// than showing an opaque "blocked".
    CONNECTION_INSECURE_TRANSPORT = 68,
    /// The host is explicitly refusing all remote access until a locally
    /// authenticated user unlocks it. This is a security state, not an offline
    /// or retryable transport failure.
    REMOTE_ACCESS_LOCKED = 69,
    /// The shared coordination store (Redis) could not be reached, so a view that
    /// is sourced from it cannot be served. Distinct from `SYSTEM_ERROR` so an
    /// operator reading the console sees an infrastructure dependency being down
    /// rather than an opaque internal failure, and so the UI can say which
    /// dependency. Carried in `RestResponse.code`, never an HTTP status.
    SHARED_STORE_UNAVAILABLE = 70,

    ACTION_NEED_RETRY = 1001,

    REMOTE_DESK_OFFLINE = 10003,
    TIMEOUT = 10004,
    SESSION_NOT_FOUND = 10005,

    /// The device's owning manager instance is not reachable for cross-instance
    /// proxying: its presence aged to stale, the instance is not live in the
    /// instance registry, it advertised no internal base URL, or the internal
    /// hop could not connect. The request never reached the device, so it is
    /// safe for the client to retry. Carried in `RestResponse.code`, never an
    /// HTTP status (rule: business errors stay HTTP 200).
    MANAGER_NODE_UNREACHABLE = 10007,
    /// A cross-instance proxied write (delete file / update settings) failed
    /// after the request was already dispatched toward the device, so the
    /// outcome is unknown — it may or may not have taken effect. The client must
    /// NOT auto-retry; it should prompt the user to refresh and confirm. Carried
    /// in `RestResponse.code`, never an HTTP status.
    REMOTE_DESK_OUTCOME_UNKNOWN = 10008,

    GENERATE_LOCAL_DESCRIPTION_FAILED = 10001,
    BLANK_SIGNALING_DATA = 10002,
    AUTO_START_ERROR = 10006,

    // for windows platform
    /// windows error code
    WINDOWS_ERROR = 100001,

    // for linux platform
    /// linux error code
    LINUX_ERROR = 200001,

    // for mac platform
    /// mac error code
    MAC_ERROR = 300001,
}

impl DeskErrorCode {
    /// Turn an integer that arrived over the wire back into the typed code.
    ///
    /// This is the decode entry point, not a way to mint a code: every code this
    /// build knows is declared in `desk_error_codes!` above, and a caller that
    /// needs a new one adds a line there instead of calling this. The name has
    /// to carry that rule because the type cannot — the argument is a plain
    /// `i32` either way, and an unrecognized value is wrapped rather than
    /// rejected on purpose, since a peer on a newer build may report a code this
    /// one has never heard of.
    pub fn from_wire(code: i32) -> Self {
        DeskErrorCode(code)
    }

    pub fn code(&self) -> i32 {
        self.0
    }
}

/// Description carried into the generated spec, so a reader of the schema knows
/// what the bare integer means without going back to the Rust source.
const DESK_ERROR_CODE_DESCRIPTION: &str = "Business error code carried in `RestResponse.code`, \
in signaling error frames and on the agent-error wire. It is never an HTTP status: \
transport-level failures are expressed by the status line, business outcomes by this value.";

impl utoipa::PartialSchema for DeskErrorCode {
    /// Describe the code as a named integer enum.
    ///
    /// `enum` carries the values and the `x-enum-varnames` extension carries the
    /// matching names; both come from `ALL` in the same order, because client
    /// generators pair the two arrays by index.
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::ObjectBuilder::new()
            .schema_type(utoipa::openapi::Type::Integer)
            .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                utoipa::openapi::KnownFormat::Int32,
            )))
            .description(Some(DESK_ERROR_CODE_DESCRIPTION))
            .enum_values(Some(
                Self::ALL
                    .iter()
                    .map(|(_, code)| *code)
                    .collect::<Vec<i32>>(),
            ))
            .extensions(Some(utoipa::openapi::extensions::Extensions::from_iter([
                (
                    "x-enum-varnames",
                    Self::ALL
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<&'static str>>(),
                ),
            ])))
            .into()
    }
}

impl utoipa::ToSchema for DeskErrorCode {}

impl Display for DeskErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code_str = format!("{}", self.0);
        f.write_str(code_str.as_str())
    }
}

#[derive(Debug)]
pub struct CustomDeskError {
    pub error_code: DeskErrorCode,
    pub message: String,
}

impl fmt::Display for CustomDeskError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Custom desk error")?;

        write!(fmt, "({}): {}", self.error_code, self.message)?;

        Ok(())
    }
}

impl CustomDeskError {
    pub fn new(error_code: DeskErrorCode, message: &str) -> CustomDeskError {
        CustomDeskError {
            error_code,
            message: message.to_string(),
        }
    }
}

#[derive(Debug)]
pub enum DeskUtilsError {
    /// A log parse level error occurred.
    ParseLevelError(log::ParseLevelError),
    /// A log set logger error occurred.
    SetLoggerError(log::SetLoggerError),
}

impl Display for DeskUtilsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DeskUtilsError::ParseLevelError(err) => err.fmt(f),
            DeskUtilsError::SetLoggerError(err) => err.fmt(f),
        }
    }
}

impl From<log::ParseLevelError> for DeskUtilsError {
    fn from(err: log::ParseLevelError) -> Self {
        DeskUtilsError::ParseLevelError(err)
    }
}

impl From<log::SetLoggerError> for DeskUtilsError {
    fn from(err: log::SetLoggerError) -> Self {
        DeskUtilsError::SetLoggerError(err)
    }
}

#[cfg(test)]
mod tests {
    use super::DeskErrorCode;

    /// Lock the numeric wire value of `REVISION_CONFLICT`. Clients and the
    /// manager both branch on this exact code, so the value is a contract and
    /// must not drift.
    #[test]
    fn revision_conflict_code_is_stable() {
        assert_eq!(DeskErrorCode::REVISION_CONFLICT.code(), 13);
    }

    /// The new code must not collide with any existing assignment around it.
    #[test]
    fn revision_conflict_code_is_distinct() {
        let others = [
            DeskErrorCode::PRECONDITION_FAILED.code(),
            DeskErrorCode::FILE_PATH_NOT_FOUND.code(),
            DeskErrorCode::CLIENT_ID_NOT_FOUND.code(),
            DeskErrorCode::NOT_ALLOW_DELETE_FILE.code(),
        ];
        assert!(!others.contains(&DeskErrorCode::REVISION_CONFLICT.code()));
    }

    /// Lock the numeric wire value of `NO_VISIBLE_TARGETS`. The manager returns it
    /// and the console branches on it, so the value is a contract.
    #[test]
    fn no_visible_targets_code_is_stable_and_distinct() {
        assert_eq!(DeskErrorCode::NO_VISIBLE_TARGETS.code(), 14);
        let others = [
            DeskErrorCode::REVISION_CONFLICT.code(),
            DeskErrorCode::PRECONDITION_FAILED.code(),
            DeskErrorCode::FILE_PATH_NOT_FOUND.code(),
            DeskErrorCode::CLIENT_ID_NOT_FOUND.code(),
        ];
        assert!(!others.contains(&DeskErrorCode::NO_VISIBLE_TARGETS.code()));
    }

    /// Lock the numeric wire values of the fleet batch-execution codes. The
    /// manager returns them and the console branches on each, so the values are a
    /// contract; they must also be mutually distinct.
    #[test]
    fn fleet_exec_codes_are_stable_and_distinct() {
        assert_eq!(DeskErrorCode::FLEET_APPROVAL_STALE.code(), 15);
        assert_eq!(DeskErrorCode::FLEET_HIGH_RISK_BLOCKED.code(), 16);
        assert_eq!(DeskErrorCode::FLEET_NOT_EXECUTABLE.code(), 17);
        assert_eq!(DeskErrorCode::FLEET_DRY_RUN_NOT_APPROVABLE.code(), 18);
        assert_eq!(DeskErrorCode::FLEET_APPROVAL_FORBIDDEN.code(), 19);
        let codes = [
            DeskErrorCode::FLEET_APPROVAL_STALE.code(),
            DeskErrorCode::FLEET_HIGH_RISK_BLOCKED.code(),
            DeskErrorCode::FLEET_NOT_EXECUTABLE.code(),
            DeskErrorCode::FLEET_DRY_RUN_NOT_APPROVABLE.code(),
            DeskErrorCode::FLEET_APPROVAL_FORBIDDEN.code(),
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "fleet codes must be distinct");
        // Distinct from the adjacent NO_VISIBLE_TARGETS contract value.
        assert!(!codes.contains(&DeskErrorCode::NO_VISIBLE_TARGETS.code()));
    }

    /// Lock the numeric wire values of the login/registration anti-abuse codes.
    /// The manager returns them in `RestResponse.code` and the console branches on
    /// each (e.g. render a CAPTCHA on `CAPTCHA_REQUIRED`, show a lockout notice on
    /// `ACCOUNT_LOCKED`), so the values are a contract and must not drift. They
    /// must also be mutually distinct and not collide with the surrounding
    /// assignments.
    #[test]
    fn auth_anti_abuse_codes_are_stable_and_distinct() {
        assert_eq!(DeskErrorCode::ILLEGAL_CREDENTIALS.code(), 30);
        assert_eq!(DeskErrorCode::ACCOUNT_LOCKED.code(), 31);
        assert_eq!(DeskErrorCode::TOO_MANY_ATTEMPTS.code(), 32);
        assert_eq!(DeskErrorCode::CAPTCHA_REQUIRED.code(), 33);
        assert_eq!(DeskErrorCode::CAPTCHA_FAILED.code(), 34);
        assert_eq!(DeskErrorCode::EMAIL_NOT_VERIFIED.code(), 35);
        assert_eq!(DeskErrorCode::EMAIL_ALREADY_REGISTERED.code(), 36);
        assert_eq!(DeskErrorCode::WEAK_PASSWORD.code(), 37);
        assert_eq!(DeskErrorCode::INVALID_OR_EXPIRED_TOKEN.code(), 38);
        let codes = [
            DeskErrorCode::ILLEGAL_CREDENTIALS.code(),
            DeskErrorCode::ACCOUNT_LOCKED.code(),
            DeskErrorCode::TOO_MANY_ATTEMPTS.code(),
            DeskErrorCode::CAPTCHA_REQUIRED.code(),
            DeskErrorCode::CAPTCHA_FAILED.code(),
            DeskErrorCode::EMAIL_NOT_VERIFIED.code(),
            DeskErrorCode::EMAIL_ALREADY_REGISTERED.code(),
            DeskErrorCode::WEAK_PASSWORD.code(),
            DeskErrorCode::INVALID_OR_EXPIRED_TOKEN.code(),
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "auth codes must be distinct");
        // Distinct from neighbouring contract values on both sides of the block.
        assert!(!codes.contains(&DeskErrorCode::FILE_CHANGED.code()));
        assert!(!codes.contains(&DeskErrorCode::ACTION_NEED_RETRY.code()));
    }

    /// Lock the numeric wire values of the organization codes. The manager returns
    /// them in `RestResponse.code` and the console branches on each, so the values
    /// are a contract and must stay distinct from the surrounding anti-abuse block.
    #[test]
    fn org_codes_are_stable_and_distinct() {
        assert_eq!(DeskErrorCode::ORG_NOT_FOUND.code(), 40);
        assert_eq!(DeskErrorCode::ORG_PERMISSION_ERROR.code(), 41);
        let codes = [
            DeskErrorCode::ORG_NOT_FOUND.code(),
            DeskErrorCode::ORG_PERMISSION_ERROR.code(),
        ];
        assert_ne!(codes[0], codes[1], "org codes must be distinct");
        // Distinct from the adjacent anti-abuse block and the next contract value.
        assert!(!codes.contains(&DeskErrorCode::RATE_LIMITED.code()));
        assert!(!codes.contains(&DeskErrorCode::ACTION_NEED_RETRY.code()));
    }

    #[test]
    fn platform_ai_abuse_codes_are_stable_and_distinct() {
        assert_eq!(DeskErrorCode::AI_CONTEXT_LIMIT_EXCEEDED.code(), 83);
        assert_eq!(DeskErrorCode::AI_PLATFORM_BUSY.code(), 84);
        assert_ne!(
            DeskErrorCode::AI_CONTEXT_LIMIT_EXCEEDED,
            DeskErrorCode::AI_PLATFORM_BUSY
        );
        assert_ne!(
            DeskErrorCode::AI_CONTEXT_LIMIT_EXCEEDED,
            DeskErrorCode::RATE_LIMITED
        );
    }
    #[test]
    fn content_safety_codes_are_stable_and_image_failures_are_distinct() {
        assert_eq!(DeskErrorCode::AI_CONTENT_BLOCKED.code(), 86);
        assert_eq!(DeskErrorCode::AI_CONTENT_SAFETY_UNAVAILABLE.code(), 87);
        assert_eq!(
            DeskErrorCode::AI_CONTENT_SAFETY_IMAGE_UNSUPPORTED.code(),
            88
        );
        assert_ne!(
            DeskErrorCode::AI_CONTENT_SAFETY_IMAGE_UNSUPPORTED,
            DeskErrorCode::AI_MODEL_IMAGE_INPUT_UNSUPPORTED
        );
        assert_ne!(
            DeskErrorCode::AI_CONTENT_BLOCKED,
            DeskErrorCode::AI_CONTENT_SAFETY_UNAVAILABLE
        );
    }

    /// Lock the numeric wire values of the per-user device-quota codes. The manager
    /// emits them in a signaling `Error(-1)` frame during the registration
    /// handshake and both the desk-server host and the personal REST surface branch
    /// on the exact `{46, 47}` fatal set, so the values are a contract and must not
    /// drift. They must also be distinct from each other and from the adjacent org
    /// block.
    #[test]
    fn device_quota_codes_are_stable_and_distinct() {
        assert_eq!(DeskErrorCode::DEVICE_QUOTA_EXCEEDED.code(), 46);
        assert_eq!(DeskErrorCode::DEVICE_CLIENT_ID_REQUIRED.code(), 47);
        assert_eq!(DeskErrorCode::API_TOKEN_QUOTA_EXCEEDED.code(), 48);
        assert_eq!(DeskErrorCode::PLAN_IN_USE.code(), 49);
        assert_eq!(DeskErrorCode::TERMINAL_COPILOT_DISABLED.code(), 50);
        assert_eq!(DeskErrorCode::AI_MODEL_NOT_CONFIGURED.code(), 51);
        assert_eq!(DeskErrorCode::PLAN_NO_PRICE.code(), 52);
        assert_eq!(DeskErrorCode::PLAN_PRICE_REQUIRED.code(), 53);
        assert_eq!(DeskErrorCode::AI_MODEL_NOT_AUTHORIZED.code(), 54);
        assert_eq!(DeskErrorCode::COPILOT_STEP_LIMIT_EXCEEDED.code(), 57);
        assert_eq!(DeskErrorCode::COPILOT_RESPONSE_TRUNCATED.code(), 58);
        assert_eq!(DeskErrorCode::COPILOT_PROTOCOL_VIOLATION.code(), 59);
        assert_eq!(DeskErrorCode::COPILOT_TURN_BUSY.code(), 60);
        assert_eq!(DeskErrorCode::COPILOT_SUBJECT_MISMATCH.code(), 61);
        assert_eq!(DeskErrorCode::AGENT_SAME_TOOL_REPEAT_LIMIT.code(), 70);
        assert_eq!(DeskErrorCode::AI_MODEL_IMAGE_INPUT_UNSUPPORTED.code(), 85);
        let codes = [
            DeskErrorCode::DEVICE_QUOTA_EXCEEDED.code(),
            DeskErrorCode::DEVICE_CLIENT_ID_REQUIRED.code(),
        ];
        assert_ne!(codes[0], codes[1], "device quota codes must be distinct");
        // Distinct from the adjacent org block and the next contract value.
        assert!(!codes.contains(&DeskErrorCode::INVITE_ALREADY_PENDING.code()));
        assert!(!codes.contains(&DeskErrorCode::ACTION_NEED_RETRY.code()));
    }

    #[test]
    fn remote_access_locked_code_is_stable() {
        assert_eq!(DeskErrorCode::REMOTE_ACCESS_LOCKED.code(), 69);
        assert_ne!(
            DeskErrorCode::REMOTE_ACCESS_LOCKED.code(),
            DeskErrorCode::REMOTE_DESK_OFFLINE.code()
        );
    }

    #[test]
    fn manager_credential_codes_are_stable_and_distinct() {
        assert_eq!(DeskErrorCode::MANAGER_CREDENTIAL_REVOKED.code(), 81);
        assert_eq!(DeskErrorCode::MANAGER_CREDENTIAL_SUSPENDED.code(), 82);
        assert_ne!(
            DeskErrorCode::MANAGER_CREDENTIAL_REVOKED.code(),
            DeskErrorCode::MANAGER_CREDENTIAL_SUSPENDED.code()
        );
        assert_ne!(
            DeskErrorCode::MANAGER_CREDENTIAL_REVOKED.code(),
            DeskErrorCode::ACTION_NEED_RETRY.code()
        );
    }

    /// `ALL` is the table the OpenAPI schema is projected from, so an entry that
    /// disagrees with its constant would ship a wrong value to every client.
    #[test]
    fn all_entries_agree_with_their_constants() {
        let lookup = |name: &str| {
            DeskErrorCode::ALL
                .iter()
                .find(|(entry, _)| *entry == name)
                .unwrap_or_else(|| panic!("{name} missing from DeskErrorCode::ALL"))
                .1
        };
        assert_eq!(lookup("SUCCESS"), DeskErrorCode::SUCCESS.code());
        assert_eq!(
            lookup("REVISION_CONFLICT"),
            DeskErrorCode::REVISION_CONFLICT.code()
        );
        assert_eq!(
            lookup("PERMISSION_ERROR"),
            DeskErrorCode::PERMISSION_ERROR.code()
        );
        assert_eq!(lookup("MAC_ERROR"), DeskErrorCode::MAC_ERROR.code());
    }

    /// Client generators pair `x-enum-varnames` with `enum` by index after
    /// de-duplicating the names, so a repeated name would silently shift every
    /// later pairing. Repeated values would collapse two codes into one member.
    #[test]
    fn all_names_and_values_are_unique() {
        let mut names: Vec<&str> = DeskErrorCode::ALL.iter().map(|(name, _)| *name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate error-code name");

        let mut values: Vec<i32> = DeskErrorCode::ALL.iter().map(|(_, code)| *code).collect();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), total, "duplicate error-code value");
    }

    /// The schema must describe an int32 enum whose two parallel arrays line up:
    /// this is exactly what the client generator consumes.
    #[test]
    fn schema_is_an_int32_enum_with_aligned_varnames() {
        use utoipa::PartialSchema;

        let schema = serde_json::to_value(DeskErrorCode::schema()).expect("schema serializes");
        assert_eq!(schema["type"], "integer");
        assert_eq!(schema["format"], "int32");

        let values = schema["enum"].as_array().expect("enum array");
        let names = schema["x-enum-varnames"]
            .as_array()
            .expect("x-enum-varnames array");
        assert_eq!(
            values.len(),
            names.len(),
            "enum and x-enum-varnames must stay the same length"
        );
        assert_eq!(values.len(), DeskErrorCode::ALL.len());

        for (index, (name, code)) in DeskErrorCode::ALL.iter().enumerate() {
            assert_eq!(names[index], *name, "name order drifted at {index}");
            assert_eq!(values[index], *code, "value order drifted at {index}");
        }
    }

    /// The platform codes are the sparse high values; a generator that collapsed
    /// the enum into an index range would drop them, so assert them explicitly.
    #[test]
    fn schema_keeps_sparse_platform_codes() {
        use utoipa::PartialSchema;

        let schema = serde_json::to_value(DeskErrorCode::schema()).expect("schema serializes");
        let values = schema["enum"].as_array().expect("enum array");
        let names = schema["x-enum-varnames"]
            .as_array()
            .expect("x-enum-varnames array");

        for (name, code) in [
            ("WINDOWS_ERROR", 100001),
            ("LINUX_ERROR", 200001),
            ("MAC_ERROR", 300001),
        ] {
            let index = names
                .iter()
                .position(|entry| entry == name)
                .unwrap_or_else(|| panic!("{name} missing from x-enum-varnames"));
            assert_eq!(values[index], code, "{name} paired with the wrong value");
        }
    }

    /// The component name is what the generated client's symbol is derived from,
    /// so renaming it would rename every consumer's import.
    #[test]
    fn schema_component_name_is_stable() {
        use utoipa::ToSchema;

        assert_eq!(DeskErrorCode::name(), "DeskErrorCode");
    }

    /// Decoding a declared code off the wire must land on the same value the
    /// constant carries, so a receiver can compare against the constant instead
    /// of the integer.
    #[test]
    fn a_declared_code_survives_the_wire() {
        let decoded = DeskErrorCode::from_wire(DeskErrorCode::REVISION_CONFLICT.code());

        assert_eq!(decoded, DeskErrorCode::REVISION_CONFLICT);
    }

    /// A peer on a newer build may report a code this one never declared. The
    /// value is kept verbatim rather than rejected or folded into a catch-all,
    /// because dropping it would lose the error the peer was reporting.
    #[test]
    fn an_undeclared_code_is_kept_verbatim() {
        let unknown = 999_999;
        assert!(
            !DeskErrorCode::ALL.iter().any(|(_, code)| *code == unknown),
            "the test's placeholder became a real code; pick another"
        );

        assert_eq!(DeskErrorCode::from_wire(unknown).code(), unknown);
    }
}
