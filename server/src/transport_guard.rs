//! Scheme-aware connect-time transport guard shared by the signaling proxy dial
//! and the connection-verify probe.
//!
//! Both dial arbitrary operator-configured signaling / manager endpoints, so both
//! must apply the same policy at the moment of connection:
//!
//! - the cloud-metadata hard floor is never reachable ([`AddressClass::AlwaysBlock`]);
//! - private / loopback / LAN targets stay reachable over plaintext (a self-hosted
//!   signaling server on a LAN commonly has no TLS);
//! - a *public* address may be refused over a plaintext scheme when the host's
//!   `require_secure_signaling` switch is on, so the API token and all signaling
//!   never cross an untrusted network in the clear.
//!
//! The judgment itself lives in [`desk_utils::ssrf::check_transport`]; this module
//! is the actix `Resolve` adapter that runs it per resolved candidate. The scheme
//! is fixed per dial and baked into the resolver ([`TransportPolicy::scheme_is_tls`]),
//! so the decision is made on a *single authoritative resolution* with no second
//! lookup that could rebind (the classic connect-time SSRF TOCTOU). Domain hosts
//! and IP literals both flow through the same filter because `lookup_host` echoes
//! literals back unchanged.
//!
//! DNS resolution is behind the [`HostResolver`] seam so the guard can be
//! unit-tested with a deterministic host→IP map (including a domain that "rebinds"
//! to an internal address) without touching the network.

use std::net::SocketAddr;
use std::rc::Rc;

use actix_tls::connect::Resolve;
use futures_util::future::LocalBoxFuture;

/// Injectable DNS resolution seam. Production uses [`SystemHostResolver`]
/// (`tokio::net::lookup_host`); tests inject a fixed map so a domain can be made
/// to resolve to an internal / metadata address deterministically.
pub trait HostResolver {
    /// Resolve `host:port` to candidate socket addresses. The returned list is the
    /// single authoritative resolution the connector will use — no address is
    /// looked up a second time.
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> LocalBoxFuture<'a, std::io::Result<Vec<SocketAddr>>>;
}

/// Production resolver over the system DNS via `tokio::net::lookup_host`.
#[derive(Clone, Copy, Default)]
pub struct SystemHostResolver;

impl HostResolver for SystemHostResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> LocalBoxFuture<'a, std::io::Result<Vec<SocketAddr>>> {
        Box::pin(async move { Ok(tokio::net::lookup_host((host, port)).await?.collect()) })
    }
}

/// The connect-time transport policy for one dial, fixed before the socket opens.
#[derive(Clone, Copy, Debug)]
pub struct TransportPolicy {
    /// Whether private / loopback / LAN targets are reachable. Signaling always
    /// allows them (a self-hosted server on a LAN); the model-provider guard sets
    /// this from its SSRF mode (`Strict` → false, `Relaxed` → true).
    pub allow_private: bool,
    /// Whether the scheme about to be dialed is TLS (`wss` / `https`). Baked in so
    /// the resolver can refuse plaintext to a public address without a second
    /// lookup.
    pub scheme_is_tls: bool,
    /// Whether public plaintext is refused (the `require_secure_signaling` /
    /// `enforce_public_tls` switch).
    pub enforce_public_tls: bool,
}

/// An actix `Resolve` that drops every resolved candidate the transport policy
/// forbids, just before the connector uses them.
#[derive(Clone)]
pub struct TransportGuardResolver {
    resolver: Rc<dyn HostResolver>,
    policy: TransportPolicy,
}

impl TransportGuardResolver {
    /// Build a guard over the production system resolver.
    pub fn system(policy: TransportPolicy) -> Self {
        Self {
            resolver: Rc::new(SystemHostResolver),
            policy,
        }
    }

    /// Build a guard over an injected resolver (tests).
    pub fn with_resolver(resolver: Rc<dyn HostResolver>, policy: TransportPolicy) -> Self {
        Self { resolver, policy }
    }
}

impl Resolve for TransportGuardResolver {
    fn lookup<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> LocalBoxFuture<'a, Result<Vec<SocketAddr>, Box<dyn std::error::Error>>> {
        let policy = self.policy;
        let resolver = Rc::clone(&self.resolver);
        Box::pin(async move {
            let resolved = resolver.resolve(host, port).await?;
            // Track whether any candidate was dropped specifically for being a
            // public plaintext target vs an SSRF-blocked address, so the coarse
            // error can hint "use TLS" without ever naming the internal host.
            let mut insecure_only = false;
            let mut allowed: Vec<SocketAddr> = Vec::new();
            for addr in resolved {
                match desk_utils::ssrf::check_transport(
                    addr.ip(),
                    policy.allow_private,
                    policy.scheme_is_tls,
                    policy.enforce_public_tls,
                ) {
                    Ok(()) => allowed.push(addr),
                    Err(desk_utils::ssrf::SsrfError::InsecureTransport) => insecure_only = true,
                    Err(_) => {}
                }
            }
            if allowed.is_empty() {
                let msg = if insecure_only {
                    // Public plaintext refused: distinct marker the probe maps to a
                    // dedicated outcome, but still no internal address leaked.
                    "target requires TLS: plaintext transport to a public address is not allowed"
                } else {
                    "host resolves to a blocked address"
                };
                return Err(Box::<dyn std::error::Error>::from(msg));
            }
            Ok(allowed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// A fake resolver returning a fixed address set for any host, so a "domain"
    /// can be made to resolve to an internal / public / metadata address.
    struct FakeResolver(Vec<SocketAddr>);

    impl HostResolver for FakeResolver {
        fn resolve<'a>(
            &'a self,
            _host: &'a str,
            _port: u16,
        ) -> LocalBoxFuture<'a, std::io::Result<Vec<SocketAddr>>> {
            let addrs = self.0.clone();
            Box::pin(async move { Ok(addrs) })
        }
    }

    fn sock(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port)
    }

    fn guard(addrs: Vec<SocketAddr>, policy: TransportPolicy) -> TransportGuardResolver {
        TransportGuardResolver::with_resolver(Rc::new(FakeResolver(addrs)), policy)
    }

    fn signaling_policy(scheme_is_tls: bool, enforce_public_tls: bool) -> TransportPolicy {
        TransportPolicy {
            allow_private: true,
            scheme_is_tls,
            enforce_public_tls,
        }
    }

    #[actix_web::test]
    async fn metadata_floor_blocked_even_with_switch_off() {
        // AlwaysBlock ignores the switch: a domain rebinding to 169.254.169.254 is
        // dropped whether the scheme is TLS or not and whether enforcement is on.
        for scheme_is_tls in [true, false] {
            let g = guard(
                vec![sock(169, 254, 169, 254, 80)],
                signaling_policy(scheme_is_tls, false),
            );
            assert!(g.lookup("metadata.example", 80).await.is_err());
        }
    }

    #[actix_web::test]
    async fn private_target_allowed_over_plaintext() {
        // A LAN address stays reachable over ws:// regardless of enforcement.
        let g = guard(
            vec![sock(192, 168, 1, 10, 80)],
            signaling_policy(false, true),
        );
        assert!(g.lookup("lan.example", 80).await.is_ok());
    }

    #[actix_web::test]
    async fn public_plaintext_blocked_when_enforced_but_tls_allowed() {
        let public = vec![sock(1, 1, 1, 1, 443)];
        // Plaintext + enforce on → all candidates dropped → error.
        let blocked = guard(public.clone(), signaling_policy(false, true));
        assert!(blocked.lookup("public.example", 443).await.is_err());
        // TLS scheme → allowed even with enforcement on.
        let tls_ok = guard(public.clone(), signaling_policy(true, true));
        assert!(tls_ok.lookup("public.example", 443).await.is_ok());
        // Plaintext + enforce off (escape hatch) → allowed.
        let escape = guard(public, signaling_policy(false, false));
        assert!(escape.lookup("public.example", 443).await.is_ok());
    }

    #[actix_web::test]
    async fn mixed_candidates_keep_only_allowed_addresses() {
        // A host resolving to both a public and a private address, dialed over
        // plaintext with enforcement on: the public candidate is dropped, the
        // private one survives, so the dial still proceeds to the LAN address.
        let g = guard(
            vec![sock(1, 1, 1, 1, 80), sock(10, 0, 0, 5, 80)],
            signaling_policy(false, true),
        );
        let allowed = g
            .lookup("split.example", 80)
            .await
            .expect("private survives");
        assert_eq!(allowed, vec![sock(10, 0, 0, 5, 80)]);
    }

    /// The load-bearing guarantee: when the guard drops every candidate, the awc
    /// connector opens **no TCP socket at all** — not merely "sends no HTTP
    /// request". Proven with a real listener whose accept count must stay 0 for a
    /// dropped candidate, while a sanity pass shows an allowed candidate does
    /// connect (so the zero is meaningful). The drop reason here is
    /// `allow_private=false`, which takes the identical `allowed.is_empty() → Err`
    /// path as the public-plaintext refusal.
    #[actix_web::test]
    async fn dropped_candidate_opens_no_socket() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = StdArc::new(AtomicUsize::new(0));
        let accepts_task = accepts.clone();
        actix_web::rt::spawn(async move {
            loop {
                if listener.accept().await.is_ok() {
                    accepts_task.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        let build_client = |allow_private: bool| {
            let policy = TransportPolicy {
                allow_private,
                scheme_is_tls: false,
                enforce_public_tls: true,
            };
            let guard =
                TransportGuardResolver::with_resolver(Rc::new(FakeResolver(vec![addr])), policy);
            let tcp =
                actix_tls::connect::Connector::new(actix_tls::connect::Resolver::custom(guard))
                    .service();
            awc::Client::builder()
                .connector(awc::Connector::new().connector(tcp))
                .finish()
        };

        // Dropped candidate: the connector must never reach the listener.
        let _ = build_client(false)
            .get(format!("http://blocked.example:{}/", addr.port()))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            0,
            "a dropped candidate must open no TCP socket"
        );

        // Sanity: an allowed candidate DOES connect, so the zero above is a real
        // signal, not a broken harness.
        let _ = build_client(true)
            .get(format!("http://allowed.example:{}/", addr.port()))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        let mut connected = false;
        for _ in 0..40 {
            if accepts.load(Ordering::SeqCst) >= 1 {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            connected,
            "an allowed candidate must open a socket (harness sanity)"
        );
    }

    #[actix_web::test]
    async fn strict_style_drops_private() {
        // allow_private=false (model-provider Strict style): a private address is
        // dropped even though it is not the metadata floor.
        let g = guard(
            vec![sock(10, 0, 0, 5, 443)],
            TransportPolicy {
                allow_private: false,
                scheme_is_tls: true,
                enforce_public_tls: true,
            },
        );
        assert!(g.lookup("internal.example", 443).await.is_err());
    }
}
