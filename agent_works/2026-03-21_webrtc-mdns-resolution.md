# 2026-03-21_webrtc-mdns-resolution

## Implementation Plan
- Identify why WebRTC ICE fails when browsers emit mDNS (.local) host candidates.
- Add server-side visibility for TURN/STUN UDP traffic and ICE candidate intake.
- Implement mDNS candidate handling and fallback rewriting to a routable IP.
- Verify behavior with local browser tests and iterate based on logs.

## Task List
- Add TURN UDP recv/send logging for visibility.
- Add server-side mDNS query + candidate rewrite.
- Add signaling-layer mDNS fallback rewrite using signaling peer IP.
- Clean up duplicate imports introduced by logging/resolution changes.

## Walkthrough
- Added TURN UDP traffic logs to confirm STUN/TURN packets were reaching the server process.
- Determined ICE failures were caused by mDNS (.local) candidates from browsers, which were not resolvable by the server.
- Implemented mDNS resolution in `server/src/service/signaling.rs` to resolve `.local` hostnames and rewrite ICE candidates to real IPs when possible.
- Added a more reliable, product-friendly fallback in `signal/src/service.rs`: when a `.local` candidate is received, rewrite it using the signaling connection’s peer IP. This avoids dependency on mDNS multicast reachability.
- Fixed duplicate `Duration`/`mpsc` imports introduced during instrumentation.

## Notes
- Sensitive values (IPs, usernames, tokens) have been redacted.
- Remaining risk: signaling peer IP fallback may be less accurate across NAT; TURN/STUN still recommended for wide-area traversal.

## Files Touched
- `turn/src/service.rs`
- `server/src/service/signaling.rs`
- `signal/src/service.rs`
- `Cargo.toml`
- `server/Cargo.toml`
