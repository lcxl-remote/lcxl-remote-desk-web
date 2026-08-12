//! Server-resolved authentication identity for a signaling connection.
//!
//! This is an **internal, server-side** identity record — it is never part of
//! any wire type and is never populated from client-supplied fields. The
//! signaling endpoint resolves it from a validated API token (DB lookup) or an
//! authenticated session cookie, then attaches it to the connection. Downstream
//! authorization (the fleet policy decision point) and audit attribution read
//! it instead of trusting anything the control end reports.

use crate::grant::GrantPrincipal;
use crate::model::signal::RemoteDeskTypeEnum;

/// How a connection authenticated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthKind {
    /// No authenticated identity (should not occur for accepted connections).
    #[default]
    None,
    /// Authenticated by an API token (desk-server / node connections).
    TokenAuth,
    /// Authenticated by a session cookie (browser / operator connections).
    CookieAuth,
    /// A capability-scoped anonymous redeemer on the open-source single-account
    /// signal: it presented a valid access-grant code and holds a server-minted
    /// `code_session_id` (in a private cookie), **not** the owner account. Its
    /// authority comes solely from the grant it redeemed, so it is never the
    /// single-account owner and every RequestRemoteAccess it makes is stamped with the
    /// code's capability ceiling — never full control.
    CodeSession,
}

/// Server-resolved identity bound to a single signaling connection.
///
/// All fields are derived from validated server-side state:
/// - `user_id` / `token_id` come from the token/session validation.
/// - `remote_desk_type` is only meaningful after validation (a control end
///   cannot escalate its type by self-reporting it).
/// - `bound_device_id` is the device registry primary key resolved by
///   owner-bound registration; only token-authenticated `Server` connections
///   carry one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthContext {
    pub auth_kind: AuthKind,
    pub user_id: Option<i32>,
    pub token_id: Option<i32>,
    pub remote_desk_type: RemoteDeskTypeEnum,
    pub bound_device_id: Option<i32>,
    /// The server-minted `code_session_id` when `auth_kind == CodeSession`. It is
    /// the grant principal source (`code:{id}`) the RequestRemoteAccess authorizer uses
    /// to look up and authorize the redeemed grant. Resolved from a private
    /// (encrypted) cookie, never self-reported.
    pub code_session_id: Option<String>,
}

impl AuthContext {
    /// An unauthenticated context (no identity resolved).
    pub fn anonymous(remote_desk_type: RemoteDeskTypeEnum) -> Self {
        Self {
            auth_kind: AuthKind::None,
            user_id: None,
            token_id: None,
            remote_desk_type,
            bound_device_id: None,
            code_session_id: None,
        }
    }

    /// A token-authenticated context (desk-server / node). `bound_device_id` is
    /// attached separately once owner-bound device registration resolves it.
    pub fn token_auth(user_id: i32, token_id: i32, remote_desk_type: RemoteDeskTypeEnum) -> Self {
        Self {
            auth_kind: AuthKind::TokenAuth,
            user_id: Some(user_id),
            token_id: Some(token_id),
            remote_desk_type,
            bound_device_id: None,
            code_session_id: None,
        }
    }

    /// A cookie-authenticated context (browser / operator). Such connections
    /// never carry a `bound_device_id` — they are control ends, not devices.
    pub fn cookie(user_id: i32, remote_desk_type: RemoteDeskTypeEnum) -> Self {
        Self {
            auth_kind: AuthKind::CookieAuth,
            user_id: Some(user_id),
            token_id: None,
            remote_desk_type,
            bound_device_id: None,
            code_session_id: None,
        }
    }

    /// A capability-scoped code-session context (open-source anonymous redeemer).
    /// It carries no `user_id` — so it can never be mistaken for the single-account
    /// owner — only the server-minted `code_session_id` that identifies its grant
    /// principal. Always a control end (`Browser`), never a device.
    pub fn code_session(code_session_id: impl Into<String>) -> Self {
        Self {
            auth_kind: AuthKind::CodeSession,
            user_id: None,
            token_id: None,
            remote_desk_type: RemoteDeskTypeEnum::Browser,
            bound_device_id: None,
            code_session_id: Some(code_session_id.into()),
        }
    }

    /// The grant principal this connection authorizes as, when it is a
    /// code-session. Owner / node / anonymous connections have no code-session
    /// principal (they authorize by other means), so this returns `None`.
    pub fn grant_principal(&self) -> Option<GrantPrincipal> {
        match (self.auth_kind, &self.code_session_id) {
            (AuthKind::CodeSession, Some(id)) => Some(GrantPrincipal::from_code_session(id)),
            _ => None,
        }
    }

    /// Attach the resolved device registry id (token-auth `Server` only).
    pub fn with_bound_device_id(mut self, device_id: Option<i32>) -> Self {
        self.bound_device_id = device_id;
        self
    }
}

impl Default for AuthContext {
    fn default() -> Self {
        Self::anonymous(RemoteDeskTypeEnum::Browser)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_auth_server_is_fully_populated() {
        let ctx = AuthContext::token_auth(7, 3, RemoteDeskTypeEnum::Server)
            .with_bound_device_id(Some(42));
        assert_eq!(ctx.auth_kind, AuthKind::TokenAuth);
        assert_eq!(ctx.user_id, Some(7));
        assert_eq!(ctx.token_id, Some(3));
        assert_eq!(ctx.remote_desk_type, RemoteDeskTypeEnum::Server);
        assert_eq!(ctx.bound_device_id, Some(42));
    }

    #[test]
    fn cookie_browser_has_user_but_no_device() {
        let ctx = AuthContext::cookie(9, RemoteDeskTypeEnum::Browser);
        assert_eq!(ctx.auth_kind, AuthKind::CookieAuth);
        assert_eq!(ctx.user_id, Some(9));
        assert_eq!(ctx.token_id, None);
        assert_eq!(ctx.bound_device_id, None);
    }

    #[test]
    fn anonymous_is_empty() {
        let ctx = AuthContext::anonymous(RemoteDeskTypeEnum::Browser);
        assert_eq!(ctx.auth_kind, AuthKind::None);
        assert_eq!(ctx.user_id, None);
        assert_eq!(ctx.token_id, None);
        assert_eq!(ctx.bound_device_id, None);
        assert_eq!(ctx.code_session_id, None);
        assert_eq!(ctx.grant_principal(), None);
    }

    /// A code-session carries its grant principal but never a `user_id`, so it can
    /// never alias the single-account owner (`user_id == 1`) on the open-source
    /// signal, and it is only ever a control-end `Browser`.
    #[test]
    fn code_session_has_principal_but_no_owner_user_id() {
        let ctx = AuthContext::code_session("sess-xyz");
        assert_eq!(ctx.auth_kind, AuthKind::CodeSession);
        assert_eq!(ctx.user_id, None);
        assert_eq!(ctx.token_id, None);
        assert_eq!(ctx.bound_device_id, None);
        assert_eq!(ctx.remote_desk_type, RemoteDeskTypeEnum::Browser);
        assert_eq!(
            ctx.grant_principal(),
            Some(GrantPrincipal::from_code_session("sess-xyz"))
        );
        // Owner / node / anonymous contexts never expose a code-session principal.
        assert_eq!(
            AuthContext::cookie(1, RemoteDeskTypeEnum::Browser).grant_principal(),
            None
        );
        assert_eq!(
            AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server).grant_principal(),
            None
        );
    }

    /// A cookie control end never gains a device binding even if asked, because
    /// `cookie()` ignores device ids by construction — only `Server` token-auth
    /// registration attaches one.
    #[test]
    fn default_is_anonymous_browser() {
        let ctx = AuthContext::default();
        assert_eq!(ctx.auth_kind, AuthKind::None);
        assert_eq!(ctx.remote_desk_type, RemoteDeskTypeEnum::Browser);
    }
}
