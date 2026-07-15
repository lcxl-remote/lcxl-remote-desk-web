//! SSRF / transport judgment core for outbound dials (single source of truth
//! shared by the manager, the open-source signal orchestrator, and the desk
//! server's signaling / connection-verify path).
//!
//! Two related concerns live here as pure functions:
//!
//! - **Model-provider SSRF** ([`ProviderSsrfMode`], [`check_provider_url`],
//!   [`check_resolved_ip`]): users configure a provider `base_url` that the server
//!   then dials. In a multi-tenant deployment any registered (untrusted) user can
//!   point that URL at an internal service or a cloud-metadata endpoint and read
//!   the response back — a classic SSRF. The mode decides whether a URL (write
//!   time) or a resolved IP (connect time, authoritative against DNS rebinding) is
//!   allowed.
//! - **Transport policy** ([`AddressClass`], [`classify_address`],
//!   [`check_transport`]): the connect-time judgment that additionally refuses a
//!   *plaintext* dial to a *public* address (used by the signaling proxy and the
//!   connection-verify probe), while always blocking the cloud-metadata floor and
//!   always permitting private / LAN targets over plaintext.
//!
//! It is intentionally dependency-light: only `url` for parsing and `std::net`
//! for address classification. No HTTP client, no TLS, no DNS, no config reading —
//! the callers own those and feed this module already-parsed inputs (the actix
//! `Resolve` adapter that runs [`check_transport`] per candidate lives in the desk
//! server, not here). Keeping it pure keeps it trivially unit-testable without a
//! network.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How aggressively to guard provider outbound dials. Configured per deployment;
/// each binary supplies its own sensible default (manager defaults to `Strict`,
/// the single-tenant open-source signal defaults to `Relaxed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSsrfMode {
    /// Public multi-tenant posture. Scheme must be `https`; private, loopback,
    /// link-local, ULA, CGNAT and cloud-metadata ranges are all rejected.
    Strict,
    /// Single-tenant / self-host posture. `http` is allowed and private/loopback
    /// targets (local model gateways like ollama / vLLM) are permitted, but the
    /// cloud-metadata hard floor is still enforced.
    Relaxed,
}

impl FromStr for ProviderSsrfMode {
    type Err = ();

    /// Parse from a config string (case-insensitive). Used when reading the mode
    /// from a stored system parameter / settings value.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "strict" => Ok(ProviderSsrfMode::Strict),
            "relaxed" => Ok(ProviderSsrfMode::Relaxed),
            _ => Err(()),
        }
    }
}

impl ProviderSsrfMode {
    /// Stable lowercase token for this mode (inverse of [`FromStr`]).
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderSsrfMode::Strict => "strict",
            ProviderSsrfMode::Relaxed => "relaxed",
        }
    }
}

impl fmt::Display for ProviderSsrfMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a provider target was rejected. Deliberately coarse: the message handed
/// back to an untrusted caller must not leak which internal host was probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsrfError {
    /// The string did not parse as a URL.
    InvalidUrl,
    /// The URL had no host component.
    MissingHost,
    /// The scheme is not permitted under the active mode (e.g. `http` in Strict,
    /// or any non-http(s) scheme).
    SchemeNotAllowed,
    /// The target address falls in a denied range for the active mode.
    BlockedAddress,
    /// A plaintext (non-TLS) scheme targets a public address while public-TLS
    /// enforcement is on. Kept distinct from [`SsrfError::BlockedAddress`] so a
    /// caller can surface a "use TLS" hint rather than an opaque "blocked".
    InsecureTransport,
}

impl fmt::Display for SsrfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            SsrfError::InvalidUrl => "provider URL is not a valid URL",
            SsrfError::MissingHost => "provider URL has no host",
            SsrfError::SchemeNotAllowed => "provider URL scheme is not allowed",
            SsrfError::BlockedAddress => "provider URL resolves to a blocked address",
            SsrfError::InsecureTransport => {
                "plaintext transport to a public address is not allowed"
            }
        };
        f.write_str(msg)
    }
}

impl std::error::Error for SsrfError {}

/// Validate a provider URL at write time (when a user saves a provider config).
///
/// Parses the scheme and host: the scheme is checked against the mode, and if the
/// host is an IP literal it is judged immediately. Domain hosts are deferred to
/// the authoritative connect-time check ([`check_resolved_ip`]) because DNS can
/// rebind between save and dial.
pub fn check_provider_url(raw: &str, mode: ProviderSsrfMode) -> Result<(), SsrfError> {
    let url = url::Url::parse(raw).map_err(|_| SsrfError::InvalidUrl)?;
    if !scheme_allowed(url.scheme(), mode) {
        return Err(SsrfError::SchemeNotAllowed);
    }
    match url.host() {
        Some(url::Host::Ipv4(v4)) => {
            if is_blocked(IpAddr::V4(v4), mode) {
                return Err(SsrfError::BlockedAddress);
            }
        }
        Some(url::Host::Ipv6(v6)) => {
            if is_blocked(IpAddr::V6(v6), mode) {
                return Err(SsrfError::BlockedAddress);
            }
        }
        // Domain hosts are resolved and re-checked at connect time.
        Some(url::Host::Domain(_)) => {}
        None => return Err(SsrfError::MissingHost),
    }
    Ok(())
}

/// Validate a single resolved IP at connect time. This is the authoritative
/// check: it runs on every candidate address the resolver returns, just before
/// the socket connects, so a domain that rebinds to an internal IP is still
/// caught.
pub fn check_resolved_ip(ip: IpAddr, mode: ProviderSsrfMode) -> Result<(), SsrfError> {
    if is_blocked(ip, mode) {
        Err(SsrfError::BlockedAddress)
    } else {
        Ok(())
    }
}

/// Whether a scheme is permitted under the active mode.
fn scheme_allowed(scheme: &str, mode: ProviderSsrfMode) -> bool {
    match mode {
        ProviderSsrfMode::Strict => scheme.eq_ignore_ascii_case("https"),
        ProviderSsrfMode::Relaxed => {
            scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http")
        }
    }
}

/// Core address judgment for the active mode, after IPv4-mapped normalization.
fn is_blocked(ip: IpAddr, mode: ProviderSsrfMode) -> bool {
    let ip = normalize(ip);
    match mode {
        ProviderSsrfMode::Relaxed => is_metadata_floor(ip),
        ProviderSsrfMode::Strict => is_metadata_floor(ip) || is_private_or_loopback(ip),
    }
}

/// Normalize IPv4-mapped IPv6 (`::ffff:a.b.c.d`) back to IPv4 so that, e.g.,
/// `[::ffff:169.254.169.254]` cannot slip past the IPv4 metadata range check.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// The cloud-metadata hard floor: rejected under both `Strict` and `Relaxed`.
/// Covers the most dangerous SSRF target (credential-bearing instance metadata)
/// plus unspecified addresses, across IPv4 and IPv6.
///
/// Maintenance model: the common case needs no per-vendor list, because the bulk
/// is the `169.254.0.0/16` link-local range holding the de-facto metadata address
/// `169.254.169.254` (shared by AWS, GCP, Azure, Oracle, Tencent Cloud, and most
/// others). Only a vendor that puts metadata on a NON-link-local address which
/// also falls inside a private/CGNAT/ULA range must be carved out individually
/// (otherwise it would be misclassified as private and let through) — the AWS
/// IPv6 address `fd00:ec2::254` below is one such carve-out.
///
/// Known outlier NOT yet covered here: Alibaba Cloud metadata `100.100.100.200`,
/// which sits inside CGNAT `100.64.0.0/10` and is therefore currently classified
/// as private rather than blocked. Whether to add it to this floor or make the
/// floor configurable is an open follow-up; the addresses are intentionally
/// hardcoded because this is a security floor an operator must not weaken.
fn is_metadata_floor(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // 0.0.0.0/8 ("this host", includes the unspecified address) and
            // 169.254.0.0/16 link-local (covers 169.254.169.254 — AWS IMDSv1/v2,
            // GCP, Azure metadata).
            o[0] == 0 || (o[0] == 169 && o[1] == 254)
        }
        IpAddr::V6(v6) => {
            // Unspecified (::), fe80::/10 link-local (covers metadata reachable
            // via link-local), and the AWS IPv6 IMDS address fd00:ec2::254 — the
            // latter sits inside ULA (fc00::/7) which Relaxed otherwise permits,
            // so it must be carved out explicitly.
            v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254)
        }
    }
}

/// Private / loopback / CGNAT / ULA ranges, rejected only under `Strict`.
fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()                                   // 127.0.0.0/8
                || v4.is_private()                             // 10/8, 172.16/12, 192.168/16
                || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10 CGNAT
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()                                   // ::1
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
        }
    }
}

/// Transport-security classification of a resolved candidate address, layered on
/// top of the SSRF ranges. Where [`check_resolved_ip`] answers "may I reach this
/// address at all", this answers "what transport policy applies", so a plaintext
/// dial to a *public* address can be blocked while private / loopback targets
/// (local gateways, self-hosted signaling on a LAN) stay reachable over plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    /// The cloud-metadata hard floor (see [`is_metadata_floor`]). Never reachable,
    /// under any mode or switch — this is a security floor, not a TLS policy.
    AlwaysBlock,
    /// A private / loopback / CGNAT / ULA address (see [`is_private_or_loopback`]).
    /// Reachable over plaintext without any TLS requirement: these are LAN /
    /// local-gateway targets where TLS is commonly absent and the traffic never
    /// crosses an untrusted network.
    PrivateExempt,
    /// Any other (public / internet-routable) address. Plaintext to such a target
    /// crosses untrusted networks, so a public-TLS switch may require TLS.
    TlsRequired,
}

/// Classify a resolved IP for transport policy. Metadata floor wins first (it must
/// never be reachable), then private/loopback, else public. The IP is IPv4-mapped
/// normalized first so `::ffff:169.254.169.254` cannot slip past the floor.
pub fn classify_address(ip: IpAddr) -> AddressClass {
    let ip = normalize(ip);
    if is_metadata_floor(ip) {
        AddressClass::AlwaysBlock
    } else if is_private_or_loopback(ip) {
        AddressClass::PrivateExempt
    } else {
        AddressClass::TlsRequired
    }
}

/// The authoritative connect-time transport judgment, run per resolved candidate
/// just before the socket connects. It composes the SSRF address floor with the
/// public-plaintext TLS policy in one place so callers cannot get the layering
/// wrong:
///
/// - [`AddressClass::AlwaysBlock`] → always rejected (`BlockedAddress`), ignoring
///   both `allow_private` and `enforce_public_tls`. The metadata floor is not a
///   TLS policy and no switch may weaken it.
/// - [`AddressClass::PrivateExempt`] → reachable iff `allow_private` (the SSRF
///   mode: `Relaxed`/signaling allow it, `Strict` does not). No TLS requirement is
///   ever imposed on private targets.
/// - [`AddressClass::TlsRequired`] → reachable over TLS always; over plaintext only
///   when `enforce_public_tls` is off (an operator escape hatch). Otherwise
///   rejected with [`SsrfError::InsecureTransport`].
///
/// `scheme_is_tls` is fixed per dial (the caller knows the scheme it is about to
/// use — `wss`/`https` vs `ws`/`http`) and baked into the resolver, so the decision
/// is made on the single authoritative resolution with no second lookup.
pub fn check_transport(
    ip: IpAddr,
    allow_private: bool,
    scheme_is_tls: bool,
    enforce_public_tls: bool,
) -> Result<(), SsrfError> {
    match classify_address(ip) {
        AddressClass::AlwaysBlock => Err(SsrfError::BlockedAddress),
        AddressClass::PrivateExempt => {
            if allow_private {
                Ok(())
            } else {
                Err(SsrfError::BlockedAddress)
            }
        }
        AddressClass::TlsRequired => {
            if scheme_is_tls || !enforce_public_tls {
                Ok(())
            } else {
                Err(SsrfError::InsecureTransport)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn mode_roundtrips_via_str() {
        for m in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            assert_eq!(ProviderSsrfMode::from_str(m.as_str()), Ok(m));
        }
        assert_eq!(
            ProviderSsrfMode::from_str("STRICT"),
            Ok(ProviderSsrfMode::Strict)
        );
        assert_eq!(
            ProviderSsrfMode::from_str(" relaxed "),
            Ok(ProviderSsrfMode::Relaxed)
        );
        assert!(ProviderSsrfMode::from_str("nonsense").is_err());
        // `off` was removed (it was an escape hatch that bypassed the metadata
        // floor); it no longer parses to any mode.
        assert!(ProviderSsrfMode::from_str("off").is_err());
    }

    #[test]
    fn mode_serde_is_lowercase() {
        let json = serde_json::to_string(&ProviderSsrfMode::Strict).unwrap();
        assert_eq!(json, "\"strict\"");
        let parsed: ProviderSsrfMode = serde_json::from_str("\"relaxed\"").unwrap();
        assert_eq!(parsed, ProviderSsrfMode::Relaxed);
    }

    #[test]
    fn metadata_floor_blocked_in_both_strict_and_relaxed() {
        for mode in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            // IPv4 cloud metadata.
            assert!(check_resolved_ip(v4(169, 254, 169, 254), mode).is_err());
            // Anything in 169.254.0.0/16.
            assert!(check_resolved_ip(v4(169, 254, 1, 1), mode).is_err());
            // 0.0.0.0/8 and unspecified.
            assert!(check_resolved_ip(v4(0, 0, 0, 0), mode).is_err());
            // IPv6 unspecified, link-local, AWS IPv6 IMDS.
            assert!(check_resolved_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED), mode).is_err());
            assert!(
                check_resolved_ip(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)), mode)
                    .is_err()
            );
            assert!(
                check_resolved_ip(
                    IpAddr::V6(Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254)),
                    mode
                )
                .is_err()
            );
        }
    }

    #[test]
    fn ipv4_mapped_metadata_is_normalized_and_blocked() {
        // [::ffff:169.254.169.254] must be caught after normalization.
        let mapped = IpAddr::V6(Ipv4Addr::new(169, 254, 169, 254).to_ipv6_mapped());
        assert!(check_resolved_ip(mapped, ProviderSsrfMode::Relaxed).is_err());
        assert!(check_resolved_ip(mapped, ProviderSsrfMode::Strict).is_err());
    }

    #[test]
    fn private_ranges_blocked_only_in_strict() {
        let privates = [
            v4(127, 0, 0, 1),  // loopback
            v4(10, 1, 2, 3),   // 10/8
            v4(172, 16, 0, 1), // 172.16/12
            v4(172, 31, 255, 1),
            v4(192, 168, 1, 1), // 192.168/16
            v4(100, 64, 0, 1),  // CGNAT
        ];
        for ip in privates {
            assert!(check_resolved_ip(ip, ProviderSsrfMode::Strict).is_err());
            // Relaxed permits local gateways.
            assert!(check_resolved_ip(ip, ProviderSsrfMode::Relaxed).is_ok());
        }
        // IPv6 loopback + ULA.
        assert!(
            check_resolved_ip(IpAddr::V6(Ipv6Addr::LOCALHOST), ProviderSsrfMode::Strict).is_err()
        );
        assert!(
            check_resolved_ip(
                IpAddr::V6(Ipv6Addr::new(0xfd12, 0, 0, 0, 0, 0, 0, 1)),
                ProviderSsrfMode::Strict
            )
            .is_err()
        );
        // But ULA (other than the carved-out IMDS addr) is allowed under Relaxed.
        assert!(
            check_resolved_ip(
                IpAddr::V6(Ipv6Addr::new(0xfd12, 0, 0, 0, 0, 0, 0, 1)),
                ProviderSsrfMode::Relaxed
            )
            .is_ok()
        );
    }

    #[test]
    fn public_addresses_allowed_everywhere() {
        for mode in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            assert!(check_resolved_ip(v4(1, 1, 1, 1), mode).is_ok());
            assert!(check_resolved_ip(v4(8, 8, 8, 8), mode).is_ok());
            assert!(
                check_resolved_ip(
                    IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111)),
                    mode
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn classify_address_three_states() {
        // Metadata floor → AlwaysBlock (both IP families).
        assert_eq!(
            classify_address(v4(169, 254, 169, 254)),
            AddressClass::AlwaysBlock
        );
        assert_eq!(classify_address(v4(0, 0, 0, 0)), AddressClass::AlwaysBlock);
        assert_eq!(
            classify_address(IpAddr::V6(Ipv6Addr::new(
                0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254
            ))),
            AddressClass::AlwaysBlock
        );
        // IPv4-mapped metadata is normalized before classification.
        assert_eq!(
            classify_address(IpAddr::V6(
                Ipv4Addr::new(169, 254, 169, 254).to_ipv6_mapped()
            )),
            AddressClass::AlwaysBlock
        );
        // Private / loopback / CGNAT / ULA → PrivateExempt.
        for ip in [
            v4(127, 0, 0, 1),
            v4(10, 1, 2, 3),
            v4(192, 168, 1, 1),
            v4(100, 64, 0, 1),
        ] {
            assert_eq!(classify_address(ip), AddressClass::PrivateExempt);
        }
        assert_eq!(
            classify_address(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            AddressClass::PrivateExempt
        );
        // Public → TlsRequired.
        assert_eq!(classify_address(v4(1, 1, 1, 1)), AddressClass::TlsRequired);
        assert_eq!(
            classify_address(IpAddr::V6(Ipv6Addr::new(
                0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111
            ))),
            AddressClass::TlsRequired
        );
    }

    #[test]
    fn check_transport_metadata_floor_never_reachable() {
        // AlwaysBlock ignores both `allow_private` and `enforce_public_tls`: every
        // combination rejects, and always as BlockedAddress (never InsecureTransport).
        for &allow_private in &[true, false] {
            for &scheme_is_tls in &[true, false] {
                for &enforce in &[true, false] {
                    assert_eq!(
                        check_transport(
                            v4(169, 254, 169, 254),
                            allow_private,
                            scheme_is_tls,
                            enforce
                        ),
                        Err(SsrfError::BlockedAddress)
                    );
                }
            }
        }
    }

    #[test]
    fn check_transport_private_gated_by_allow_private_only() {
        // Private targets: reachable over plaintext when allowed, never subject to
        // the TLS switch (a LAN gateway on http is fine).
        assert!(check_transport(v4(192, 168, 1, 10), true, false, true).is_ok());
        assert!(check_transport(v4(192, 168, 1, 10), true, false, false).is_ok());
        // When private is disallowed (Strict-style), blocked regardless of scheme.
        assert_eq!(
            check_transport(v4(192, 168, 1, 10), false, true, false),
            Err(SsrfError::BlockedAddress)
        );
    }

    #[test]
    fn check_transport_public_plaintext_gated_by_switch() {
        let public = v4(1, 1, 1, 1);
        // TLS scheme: always allowed, switch on or off.
        assert!(check_transport(public, true, true, true).is_ok());
        assert!(check_transport(public, true, true, false).is_ok());
        // Plaintext + switch on → InsecureTransport (distinct from BlockedAddress).
        assert_eq!(
            check_transport(public, true, false, true),
            Err(SsrfError::InsecureTransport)
        );
        // Plaintext + switch off → escape hatch allows it.
        assert!(check_transport(public, true, false, false).is_ok());
    }

    #[test]
    fn strict_requires_https_scheme() {
        assert_eq!(
            check_provider_url("http://api.example.com/v1", ProviderSsrfMode::Strict),
            Err(SsrfError::SchemeNotAllowed)
        );
        assert!(check_provider_url("https://api.example.com/v1", ProviderSsrfMode::Strict).is_ok());
    }

    #[test]
    fn relaxed_allows_http_scheme() {
        assert!(check_provider_url("http://localhost:11434/v1", ProviderSsrfMode::Relaxed).is_ok());
        assert!(
            check_provider_url("https://api.example.com/v1", ProviderSsrfMode::Relaxed).is_ok()
        );
    }

    #[test]
    fn non_http_schemes_rejected() {
        for mode in [ProviderSsrfMode::Strict, ProviderSsrfMode::Relaxed] {
            assert_eq!(
                check_provider_url("file:///etc/passwd", mode),
                Err(SsrfError::SchemeNotAllowed)
            );
            assert_eq!(
                check_provider_url("gopher://127.0.0.1/", mode),
                Err(SsrfError::SchemeNotAllowed)
            );
        }
    }

    #[test]
    fn url_with_ip_literal_judged_at_write_time() {
        // Strict rejects an https URL whose host is a private IP literal.
        assert_eq!(
            check_provider_url("https://192.168.1.10/v1", ProviderSsrfMode::Strict),
            Err(SsrfError::BlockedAddress)
        );
        // Relaxed permits the same private literal (local gateway).
        assert!(check_provider_url("https://192.168.1.10/v1", ProviderSsrfMode::Relaxed).is_ok());
        // Metadata literal rejected even under Relaxed (and over http).
        assert_eq!(
            check_provider_url(
                "http://169.254.169.254/latest/meta-data/",
                ProviderSsrfMode::Relaxed
            ),
            Err(SsrfError::BlockedAddress)
        );
        // Bracketed IPv6 metadata literal.
        assert_eq!(
            check_provider_url("https://[fd00:ec2::254]/", ProviderSsrfMode::Relaxed),
            Err(SsrfError::BlockedAddress)
        );
    }

    #[test]
    fn url_with_domain_host_deferred_to_connect_time() {
        // A domain literal passes write-time (scheme ok); the connect-time IP
        // check is where rebinding to an internal address is caught.
        assert!(check_provider_url("https://api.openai.com/v1", ProviderSsrfMode::Strict).is_ok());
        assert!(
            check_provider_url("http://my-gateway.local/v1", ProviderSsrfMode::Relaxed).is_ok()
        );
    }

    #[test]
    fn invalid_url_rejected_when_validating() {
        assert_eq!(
            check_provider_url("definitely not a url", ProviderSsrfMode::Strict),
            Err(SsrfError::InvalidUrl)
        );
    }
}
