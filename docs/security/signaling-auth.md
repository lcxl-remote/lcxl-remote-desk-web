# Signaling Authentication

The project has connection paths with **different authentication mechanisms** that must never be confused.

| Connection | Authentication |
|---|---|
| Desk Server → Local Signaling | `settings.system.local_signaling_token` (auto-generated; present in any mode that embeds a signaling server: `default`, `signaling`, `service-daemon`) |
| Desk Server → Remote Signaling | `?token=<settings.system.signaling_token>` in the WebSocket URL |
| Desk Server → Manager | `?token=<settings.system.manager_api_token>` in the WebSocket URL |
| Browser → Signaling / Manager | **No token parameter.** Uses Actix-Session cookie authentication only. |

## Role adjudication

The signaling server decides each connection's `remote_desk_type` itself — a client's self-reported role is never trusted:

- A connection presenting a **valid** node token authenticates as its reported role (a desk-server host reports `server`).
- A connection presenting a **non-empty but invalid** token is rejected with **401** — it is *not* silently downgraded to a cookie/Browser session. This lets a client whose token went stale clear it and re-issue, instead of looping as an anonymous Browser.
- A connection with **no token** authenticates via the session cookie and is always a `browser`, regardless of any self-reported role.

The same contract holds on both the open-source signaling server and the enterprise manager.

## Access-grant sessions (device & support codes)

A controller reaches an online host by redeeming an **access code** in the connect box (`POST /api/desk/redeem-code`) — either a permanent **device code** or a **support code** (a device code with a bounded TTL, minted for a one-time assist). A redeemed session connects to the host's **regular live connection**; there is no separate "support" upstream.

Because a redeemed session is not the owner, it is **capability-scoped** and enforced **fail-closed by the host**, not by the signaling server:

- **A per-code capability ceiling travels with the session.** The owner configures, per code, a three-state ceiling (allow / ask / deny) over remote control, clipboard, private screen, whiteboard, terminal, file browse and file transfer. An unconfigured code defaults to all-ask, never to full control. See [Access Codes](/guide/access-codes).
- **The effective permission is a three-way meet.** For each action the host takes the stricter of the code's ceiling and its own global `[security]` settings, then — if that result is still "ask" — prompts the local user for live approval. `Deny` anywhere hard-denies; an unset dimension always prompts. (This replaces an earlier fixed rule that denied clipboard, file transfer and whiteboard outright.)
- **Privileged signaling is allow-listed.** Only session-establishment and control-plane frames pass on a scoped session; any frame that could leak host credentials (the `signaling_token` / `manager_api_token`) is denied at the host's signaling gate.
- **The session is time-boxed.** A support code carries a TTL; the host's local user can also end it at any time, and closing it cleans up only that connection.

Minting a support code is a **central-brain (manager) capability** — the open-source signaling server routes connections but does **not** originate support codes. On a plain signaling server the `Support` role is ordinary routing-only (equivalent to a `Browser`: no device presence, no special privileges); device-code redemption and the capability-scoped enforcement above are part of the open-source baseline regardless.

## Obtaining a host token (`POST /api/tokens`)

A logged-in client that wants to connect as a host (`remote_desk_type=server`) obtains the token it must present via `POST /api/tokens` (called on the session cookie). The endpoint exists with the same request/response shape on both backends, so a client needs no server-type probe:

- **Open-source desk-server** returns the co-located `local_signaling_token` (the same secret the embedded host worker uses). It ignores the request `name` and stores nothing. Registered only in modes with an embedded signaling server (`default` / `signaling` / `service-daemon`); a pure `desk-server` does not offer it.
- **Enterprise manager** mints a per-user token in its token table.

## Outbound transport security

The tokens above authenticate the host to the signaling server / manager, but a token is only as safe as the transport carrying it. When a desk-server dials **out** to a remote signaling server or a manager, the outbound connection is guarded at **connect time** on the resolved IP:

- **Cloud-metadata floor (always blocked).** The link-local metadata range (`169.254.0.0/16`, including `169.254.169.254`) and equivalents are never dialed, under any setting. No switch weakens this.
- **Private / loopback / LAN (always allowed over plaintext).** A self-hosted signaling server on `127.0.0.1`, `192.168.x.x`, `10.x`, etc. commonly has no TLS and its traffic never crosses an untrusted network, so plaintext (`ws://` / `http://`) is permitted.
- **Public addresses (TLS required by default).** When `require_secure_signaling` is on (the default), a **public** target dialed over a plaintext scheme is **refused before any TCP connection is made**, so the access token and all signaling are never sent in the clear across the internet. Use `wss://` / `https://`, or — for a deliberate trusted-network exception — turn the switch off in the **Desk Connection** settings page (`system.require_secure_signaling = false`).

Because the check runs on the resolved IP at the moment of connection (not on the URL string), a domain that resolves to a public address cannot bypass it, and a domain that later rebinds to an internal address is caught on the next dial. The scheme is fixed per dial, so the plaintext-vs-TLS decision is made on a single authoritative DNS resolution with no second lookup. The onboarding wizard and the **Desk Connection** page surface a public-plaintext refusal with an actionable message (switch to `wss://`/`https://`, or disable enforcement).

This outbound guard is separate from the model-provider SSRF guard (`ProviderSsrfMode`, `strict` / `relaxed`), which governs a different outbound path (the AI model API) and its own private-address posture.

## Notes

- **Browser connections do not carry a token.** They authenticate with the Actix-Session cookie. Route extractors must use `Option<web::Query<VersionInfo>>`, and manager signaling routes are excluded from the global Session middleware.
- The `local_signaling_token` is a host credential. It is masked in logs (the `SystemSettings` `Debug` output redacts it and the other secrets) and is never logged in plaintext.
- Tokens for remote signaling and the manager are passed in the WebSocket URL query string.

These mechanisms are distinct on purpose; mixing them up is a security defect.
