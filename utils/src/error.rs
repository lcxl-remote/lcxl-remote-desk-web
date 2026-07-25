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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeskErrorCode(i32);

impl DeskErrorCode {
    pub const SUCCESS: DeskErrorCode = DeskErrorCode(0);
    pub const SYSTEM_ERROR: DeskErrorCode = DeskErrorCode(1);
    pub const INVALID_STATE: DeskErrorCode = DeskErrorCode(2);
    pub const NOT_IMPLEMENTED_YET: DeskErrorCode = DeskErrorCode(3);
    pub const PERMISSION_ERROR: DeskErrorCode = DeskErrorCode(4);
    pub const INVALID_PARAMS: DeskErrorCode = DeskErrorCode(5);
    pub const UNKNOWN_SIGNALING_TYPE: DeskErrorCode = DeskErrorCode(6);
    /// The requested feature/backend is structurally unavailable in the
    /// current process or desktop context (e.g. Windows.Graphics.Capture
    /// under the SYSTEM token / Winlogon desktop, where RuntimeBroker is
    /// not running). Callers may transparently fall back to an
    /// alternative implementation instead of surfacing this as a hard
    /// error.
    pub const FEATURE_UNAVAILABLE: DeskErrorCode = DeskErrorCode(7);
    /// The request is structurally valid but rejected because a
    /// hard precondition is unmet (e.g. the caller asked the daemon
    /// to enable the virtual display but the IDD driver is not
    /// staged). Use this when the right resolution is "make the
    /// precondition true and retry," not "fix the request body."
    pub const PRECONDITION_FAILED: DeskErrorCode = DeskErrorCode(8);

    pub const FILE_PATH_NOT_FOUND: DeskErrorCode = DeskErrorCode(11);
    pub const CLIENT_ID_NOT_FOUND: DeskErrorCode = DeskErrorCode(12);
    /// Optimistic-concurrency conflict: a write supplied an `expected_revision`
    /// that no longer matches the current persisted revision (another writer or
    /// instance committed in between). The caller should re-read the current
    /// revision/value — returned in the response payload — and retry. This is a
    /// business-level outcome carried in the `RestResponse.code`, never an HTTP
    /// status code.
    pub const REVISION_CONFLICT: DeskErrorCode = DeskErrorCode(13);
    /// A fleet (multi-device) request resolved to zero diagnosable targets after
    /// applying the caller's policy visibility. Returned uniformly whether the
    /// selector matched nothing or every match was policy-invisible, so it leaks
    /// no information about devices the caller cannot see. Carried in
    /// `RestResponse.code`, never an HTTP status.
    pub const NO_VISIBLE_TARGETS: DeskErrorCode = DeskErrorCode(14);

    // ---- Fleet batch execution (write path) ----
    /// A batch approval no longer matches the previewed plan: the draft fingerprint
    /// set, `preview_generation`, or one of the bound revisions
    /// (policy / template / guardrail) drifted between preview and the approval /
    /// execution attempt. The whole batch is stale and must be re-previewed; never
    /// a partial silent drop. Carried in `RestResponse.code`, never an HTTP status.
    pub const FLEET_APPROVAL_STALE: DeskErrorCode = DeskErrorCode(15);
    /// A high-risk batch did not satisfy the guardrail (blast-radius cap exceeded,
    /// or the required two-person review was not met). Carried in
    /// `RestResponse.code`, never an HTTP status.
    pub const FLEET_HIGH_RISK_BLOCKED: DeskErrorCode = DeskErrorCode(16);
    /// A batch execution preview resolved to zero executable targets (every device
    /// was not-executable / blocked / denied), so no execution task is created.
    /// Carried in `RestResponse.code`, never an HTTP status.
    pub const FLEET_NOT_EXECUTABLE: DeskErrorCode = DeskErrorCode(17);
    /// An approve / execute action was attempted on a dry-run task, which has no
    /// execution path by construction. Carried in `RestResponse.code`, never an
    /// HTTP status.
    pub const FLEET_DRY_RUN_NOT_APPROVABLE: DeskErrorCode = DeskErrorCode(18);
    /// The approver lacks `shell.exec.confirmed` on at least one covered device, so
    /// the whole approval fails (the approved set must equal exactly the previewed
    /// draft set — never silently narrowed). Distinct from a stale approval.
    /// Carried in `RestResponse.code`, never an HTTP status.
    pub const FLEET_APPROVAL_FORBIDDEN: DeskErrorCode = DeskErrorCode(19);
    /// A device lookup in the owner-scoped personal API found no live device with
    /// the given id owned by the requesting user. Returned uniformly whether the
    /// device does not exist, was soft-deleted, or belongs to another owner, so a
    /// personal user cannot probe other owners' device ids. Carried in
    /// `RestResponse.code`, never an HTTP status.
    pub const DEVICE_NOT_FOUND: DeskErrorCode = DeskErrorCode(20);

    pub const NOT_ALLOW_DELETE_FILE: DeskErrorCode = DeskErrorCode(21);
    pub const FILE_CHANGED: DeskErrorCode = DeskErrorCode(22);

    // ---- Login / registration anti-abuse (auth hardening) ----
    /// Authentication failed. Returned uniformly for every credential-rejection
    /// cause — unknown username, wrong password, or an account that is not active
    /// — so the response leaks nothing about which accounts exist or their state.
    /// The login path equalizes its work (a dummy password verify on the failure
    /// branch) so timing does not distinguish these cases either. Carried in
    /// `RestResponse.code`, never an HTTP status.
    pub const ILLEGAL_CREDENTIALS: DeskErrorCode = DeskErrorCode(30);
    /// The target account or client IP is temporarily locked after too many
    /// failed login attempts. The lock has a TTL and (for the username dimension)
    /// can be cleared by an administrator. Carried in `RestResponse.code`, never
    /// an HTTP status.
    pub const ACCOUNT_LOCKED: DeskErrorCode = DeskErrorCode(31);
    /// A rate limit was exceeded (e.g. registration attempts per IP, or
    /// verification-email resends per address). The caller should slow down and
    /// retry later. Carried in `RestResponse.code`, never an HTTP status.
    pub const TOO_MANY_ATTEMPTS: DeskErrorCode = DeskErrorCode(32);
    /// A human-verification (CAPTCHA) challenge is now required before the request
    /// can proceed — typically after the login failure count crosses the soft
    /// threshold. The client should render the challenge and resubmit with a
    /// token. Carried in `RestResponse.code`, never an HTTP status.
    pub const CAPTCHA_REQUIRED: DeskErrorCode = DeskErrorCode(33);
    /// A supplied human-verification token was missing, malformed, or rejected by
    /// the verifier (including fail-closed when the verifier is unreachable).
    /// Carried in `RestResponse.code`, never an HTTP status.
    pub const CAPTCHA_FAILED: DeskErrorCode = DeskErrorCode(34);
    /// The account exists but its email address has not been verified, so the
    /// requested action is refused. Surfaced only by the explicit verification /
    /// resend flows; the ordinary login path stays generic (`ILLEGAL_CREDENTIALS`)
    /// to avoid account-state enumeration. Carried in `RestResponse.code`, never
    /// an HTTP status.
    pub const EMAIL_NOT_VERIFIED: DeskErrorCode = DeskErrorCode(35);
    /// Registration was refused because the (canonicalized) email or username is
    /// already taken. Returned with generic wording so it cannot be used to probe
    /// which addresses are registered. Carried in `RestResponse.code`, never an
    /// HTTP status.
    pub const EMAIL_ALREADY_REGISTERED: DeskErrorCode = DeskErrorCode(36);
    /// The supplied password did not meet the configured strength policy (length,
    /// character classes, upper bound). Carried in `RestResponse.code`, never an
    /// HTTP status.
    pub const WEAK_PASSWORD: DeskErrorCode = DeskErrorCode(37);
    /// A single-use token (email verification or password reset) was invalid,
    /// already consumed, or expired. Returned uniformly for all three so it
    /// reveals nothing about token existence. Carried in `RestResponse.code`,
    /// never an HTTP status.
    pub const INVALID_OR_EXPIRED_TOKEN: DeskErrorCode = DeskErrorCode(38);

    /// A request was throttled by a per-subject quota (e.g. too many terminal
    /// copilot asks in the window). Carried in `RestResponse.code` / streamed in a
    /// terminal AI error event, never an HTTP status; the client backs off and
    /// retries.
    pub const RATE_LIMITED: DeskErrorCode = DeskErrorCode(39);

    // ---- Organization (multi-tenant) ----
    /// An organization-scoped request referenced an org the caller cannot access:
    /// the org does not exist, was soft-deleted, or the caller is not a member.
    /// Returned uniformly for all three so a non-member cannot probe which org ids
    /// exist. Carried in `RestResponse.code`, never an HTTP status.
    pub const ORG_NOT_FOUND: DeskErrorCode = DeskErrorCode(40);
    /// The caller is a member of the organization but lacks the in-org role
    /// required for the action (e.g. a plain member attempting an org-admin write,
    /// or demoting/removing the last remaining owner). Carried in
    /// `RestResponse.code`, never an HTTP status.
    pub const ORG_PERMISSION_ERROR: DeskErrorCode = DeskErrorCode(41);
    /// An org admin tried to invite a user id that does not reference an existing
    /// user. Carried in `RestResponse.code`, never an HTTP status.
    pub const USER_NOT_FOUND: DeskErrorCode = DeskErrorCode(42);
    /// An accept/decline referenced no pending invite for the caller in that org
    /// (never invited, already responded, or revoked). Also returned to an org
    /// admin revoking a non-existent invite. Carried in `RestResponse.code`.
    pub const INVITE_NOT_FOUND: DeskErrorCode = DeskErrorCode(43);
    /// An invite targets a user who is already a member of the organization.
    /// Carried in `RestResponse.code`, never an HTTP status.
    pub const ALREADY_ORG_MEMBER: DeskErrorCode = DeskErrorCode(44);
    /// An invite already exists and is pending for this `(org, user)` pair.
    /// Carried in `RestResponse.code`, never an HTTP status.
    pub const INVITE_ALREADY_PENDING: DeskErrorCode = DeskErrorCode(45);

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
    pub const DEVICE_QUOTA_EXCEEDED: DeskErrorCode = DeskErrorCode(46);
    /// A token-authenticated desk-server registration handshake arrived without a
    /// non-empty `client_id`. Such a connection would otherwise bypass the device
    /// quota entirely (it neither registers a device nor counts against the cap),
    /// so it is rejected outright. Distinct from `DEVICE_QUOTA_EXCEEDED` so the
    /// host can show "missing device identity" rather than "device limit reached."
    /// Delivered in a signaling `Error(-1)` frame; also a fatal registration
    /// outcome that stops auto-reconnect. Never an HTTP status.
    pub const DEVICE_CLIENT_ID_REQUIRED: DeskErrorCode = DeskErrorCode(47);

    /// A new API token could not be created because the user already holds the
    /// maximum number of non-expired tokens (enabled or disabled both occupy a
    /// slot; only deleting one frees capacity). The cap shares the device-quota
    /// default threshold. The console surfaces the message; an auto-creating host
    /// client (e.g. the mobile host) treats it as a stop signal and prompts the
    /// user to remove a token or supply an existing one. Carried in
    /// `RestResponse.code`, never an HTTP status.
    pub const API_TOKEN_QUOTA_EXCEEDED: DeskErrorCode = DeskErrorCode(48);

    /// A subscription plan could not be physically deleted because one or more
    /// subscription segments still reference it (deleting the row would orphan
    /// their immutable snapshots and break the audit trail). The admin should
    /// disable the plan (`enabled = false`) instead, which stops new
    /// subscriptions while preserving history. Carried in `RestResponse.code`,
    /// never an HTTP status.
    pub const PLAN_IN_USE: DeskErrorCode = DeskErrorCode(49);

    /// The terminal copilot is turned off for this deployment (the fleet-wide
    /// enable flag is unset), so a copilot ask is refused. The control end maps
    /// this code to a localized message; the backend never sends a localized
    /// string. Rides the agent-error wire, not an HTTP status.
    pub const TERMINAL_COPILOT_DISABLED: DeskErrorCode = DeskErrorCode(50);
    /// No AI model provider is configured on the manager (provider / model /
    /// base URL / API key unset), so an agentic ask cannot dial a model. The
    /// control end maps this code to a localized "configure a model" message.
    /// Rides the agent-error wire, not an HTTP status.
    pub const AI_MODEL_NOT_CONFIGURED: DeskErrorCode = DeskErrorCode(51);
    /// The caller explicitly requested a model that is not in its resolution
    /// subject's gated catalog (its own tier plus the platform tier when the
    /// platform-fallback switch is on) — an out-of-catalog / disabled / archived
    /// model id. The request is rejected fail-closed rather than silently
    /// downgraded to a default. Rides the agent-error wire, not an HTTP status.
    pub const AI_MODEL_NOT_AUTHORIZED: DeskErrorCode = DeskErrorCode(54);

    /// A priced subscription plan has no recurring-price row matching the
    /// account's currency (neither an org-scoped override nor the platform
    /// default), so the subscription cannot snapshot a fee. The admin must add a
    /// price in that currency (or switch the plan to `free`). Carried in
    /// `RestResponse.code`; self-healing subscribe paths log a warning and skip.
    /// Never an HTTP status.
    pub const PLAN_NO_PRICE: DeskErrorCode = DeskErrorCode(52);
    /// A plan price row cannot be deleted because it is the required platform
    /// price (in the account default currency) of an enabled or default priced
    /// plan; removing it would drop the plan's default subscription back to
    /// `PLAN_NO_PRICE`. The admin must first disable the plan, switch it to
    /// `free`, or add a replacement price. Carried in `RestResponse.code`, never
    /// an HTTP status.
    pub const PLAN_PRICE_REQUIRED: DeskErrorCode = DeskErrorCode(53);
    /// A billing account cannot be switched to `prepaid` settlement because it still
    /// carries outstanding payable `point_debt`. Prepaid accounts are never billed by
    /// settlement, so the residual debt would strand forever; the admin must settle or
    /// absorb it before switching. Carried in `RestResponse.code`, never an HTTP status.
    pub const SETTLEMENT_DEBT_OUTSTANDING: DeskErrorCode = DeskErrorCode(55);

    /// No billing account exists for the referenced subject. Carried in
    /// `RestResponse.code`, never an HTTP status.
    pub const BILLING_ACCOUNT_NOT_FOUND: DeskErrorCode = DeskErrorCode(56);

    /// The agentic terminal copilot exhausted its per-turn step budget before
    /// producing an answer (the loop's step circuit-breaker tripped). The control
    /// end maps this code to a localized "ran out of steps" message. Rides the
    /// agent-error wire, not an HTTP status.
    pub const COPILOT_STEP_LIMIT_EXCEEDED: DeskErrorCode = DeskErrorCode(57);
    /// The terminal copilot's response was truncated before it completed. The
    /// control end maps this code to a localized message. Rides the agent-error
    /// wire, not an HTTP status.
    pub const COPILOT_RESPONSE_TRUNCATED: DeskErrorCode = DeskErrorCode(58);
    /// The model violated the copilot response contract (unparseable / malformed
    /// tool or answer envelope). The control end maps this code to a localized
    /// message. Rides the agent-error wire, not an HTTP status.
    pub const COPILOT_PROTOCOL_VIOLATION: DeskErrorCode = DeskErrorCode(59);
    /// Another copilot turn is already in progress for this conversation, so the
    /// new ask is refused. The control end maps this code to a localized message.
    /// Rides the agent-error wire, not an HTTP status.
    pub const COPILOT_TURN_BUSY: DeskErrorCode = DeskErrorCode(60);
    /// The copilot conversation belongs to a different session subject than the
    /// caller (a stale or cross-session continuation). The control end maps this
    /// code to a localized message. Rides the agent-error wire, not an HTTP status.
    pub const COPILOT_SUBJECT_MISMATCH: DeskErrorCode = DeskErrorCode(61);
    /// The agent stopped a turn because the model requested the same tool more
    /// times than the per-turn repeat circuit breaker permits. The control end
    /// maps this code to a localized loop-prevention message. Rides the
    /// agent-error wire, not an HTTP status.
    pub const AGENT_SAME_TOOL_REPEAT_LIMIT: DeskErrorCode = DeskErrorCode(70);
    /// The account is in a self-deletion state (`email_pending` / `grace` /
    /// `deleting` / `deleted`) and the requested mutating action is refused while
    /// the deletion is pending. The user must cancel the deletion first. Carried
    /// in `RestResponse.code` (business error, HTTP stays 200).
    pub const ACCOUNT_PENDING_DELETION: DeskErrorCode = DeskErrorCode(62);
    /// The account cannot be deleted because it still owns one or more
    /// organizations; ownership must be transferred or the organizations disbanded
    /// first. Carried in `RestResponse.code` (business error, HTTP stays 200).
    pub const ACCOUNT_STILL_ORG_OWNER: DeskErrorCode = DeskErrorCode(63);

    /// A connection-verify probe could not reach the target at all (DNS failure,
    /// connection refused, TLS handshake failure). Carried inside the
    /// `ConnectionVerifyResult` for display.
    pub const CONNECTION_UNREACHABLE: DeskErrorCode = DeskErrorCode(64);
    /// A connection-verify probe reached an endpoint but it did not identify
    /// itself as a desk signaling endpoint (missing probe marker header), so it is
    /// not usable as a signaling / manager target.
    pub const CONNECTION_NOT_SIGNALING: DeskErrorCode = DeskErrorCode(65);
    /// A connection-verify probe reached the signaling endpoint but the API token
    /// was rejected (or absent).
    pub const CONNECTION_AUTH_FAILED: DeskErrorCode = DeskErrorCode(66);
    /// A connection-verify target was refused before dialing: an unsupported URL
    /// scheme, or an address blocked by the SSRF guard.
    pub const CONNECTION_TARGET_BLOCKED: DeskErrorCode = DeskErrorCode(67);
    /// A connection-verify target resolved to a public address dialed over a
    /// plaintext scheme (`ws://` / `http://`) while `require_secure_signaling` is
    /// on. Distinct from `CONNECTION_TARGET_BLOCKED` so the wizard can prompt the
    /// user to use TLS (`wss://`) or, deliberately, disable the switch — rather
    /// than showing an opaque "blocked".
    pub const CONNECTION_INSECURE_TRANSPORT: DeskErrorCode = DeskErrorCode(68);
    /// The host is explicitly refusing all remote access until a locally
    /// authenticated user unlocks it. This is a security state, not an offline
    /// or retryable transport failure.
    pub const REMOTE_ACCESS_LOCKED: DeskErrorCode = DeskErrorCode(69);

    pub const ACTION_NEED_RETRY: DeskErrorCode = DeskErrorCode(1001);

    pub const REMOTE_DESK_OFFLINE: DeskErrorCode = DeskErrorCode(10003);
    pub const TIMEOUT: DeskErrorCode = DeskErrorCode(10004);
    pub const SESSION_NOT_FOUND: DeskErrorCode = DeskErrorCode(10005);

    /// The device's owning manager instance is not reachable for cross-instance
    /// proxying: its presence aged to stale, the instance is not live in the
    /// instance registry, it advertised no internal base URL, or the internal
    /// hop could not connect. The request never reached the device, so it is
    /// safe for the client to retry. Carried in `RestResponse.code`, never an
    /// HTTP status (rule: business errors stay HTTP 200).
    pub const MANAGER_NODE_UNREACHABLE: DeskErrorCode = DeskErrorCode(10007);
    /// A cross-instance proxied write (delete file / update settings) failed
    /// after the request was already dispatched toward the device, so the
    /// outcome is unknown — it may or may not have taken effect. The client must
    /// NOT auto-retry; it should prompt the user to refresh and confirm. Carried
    /// in `RestResponse.code`, never an HTTP status.
    pub const REMOTE_DESK_OUTCOME_UNKNOWN: DeskErrorCode = DeskErrorCode(10008);

    pub const GENERATE_LOCAL_DESCRIPTION_FAILED: DeskErrorCode = DeskErrorCode(10001);
    pub const BLANK_SIGNALING_DATA: DeskErrorCode = DeskErrorCode(10002);
    pub const AUTO_START_ERROR: DeskErrorCode = DeskErrorCode(10006);

    // for windows platform
    /// windows error code
    pub const WINDOWS_ERROR: DeskErrorCode = DeskErrorCode(100001);

    // for linux platform
    /// linux error code
    pub const LINUX_ERROR: DeskErrorCode = DeskErrorCode(200001);

    // for mac platform
    /// mac error code
    pub const MAC_ERROR: DeskErrorCode = DeskErrorCode(300001);

    pub fn new(code: i32) -> Self {
        DeskErrorCode(code)
    }

    pub fn code(&self) -> i32 {
        self.0
    }
}

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
}
