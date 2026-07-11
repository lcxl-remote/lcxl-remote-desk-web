# Access Codes

An **access code** lets a controller reach an online host by typing a short code into the connect box — no need to know the host's address. There are two kinds, and the **same input box accepts both**; the server detects which was entered:

- **Device code** — a permanent code shown by every online host. Use it to reconnect to a host at any time.
- **Support code** — a device code with a bounded lifetime (TTL), minted on demand for a one-time assist. Minting a support code requires a **central brain** (the manager); a plain signaling server routes connections but does not issue support codes.

## Redeeming a code

Enter the code in the connect box and submit (`POST /api/desk/redeem-code`). The server resolves it to the target host's live connection and returns a **capability-scoped session**. On the open-source signaling server a redeemed session is **always capability-scoped** — full, unrestricted control belongs only to the single-account owner signed in directly to the console (session cookie), not to any redeemed code.

Each redemption carries an **access ceiling** — the capability limit the owner configured for that code (see below). The ceiling travels with every control request the controller makes during the session.

## Per-code capability ceiling

The owner configures, per device code, a **capability ceiling** that bounds what a redeemed (non-owner) session may do. Each capability is three-state:

- **Allow** — permitted (still subject to the host's own global access settings).
- **Ask** — the host's local user is prompted at the moment the action is attempted.
- **Deny** — hard-denied.

Presets make this quick — **View only** (everything denied: screen view and read-only diagnosis), **Assist** (a middle ground), **Full** (everything allowed) — or set each capability individually: remote control, clipboard, private screen, whiteboard, terminal, file browse, file transfer.

A code with **no** configured ceiling defaults to the restrictive **all-ask** posture (every capability prompts), never to full control.

## How the effective permission is decided

A code's ceiling is only the upper bound. The capability actually in force for each action is the **meet (the stricter) of three things**:

1. the **code's ceiling** — what the owner allowed for this code;
2. the host's **global access settings** — `[security]` in `config.toml`; and
3. the **live approval** at the host — when the meet of (1) and (2) is left unset ("ask"), the host's local user is prompted, and may choose to remember the answer.

So **Deny** anywhere wins (hard-denied); an unset dimension on either side always prompts; and **Allow** on both the ceiling and the global setting is the only combination that passes without a prompt. Enforcement is entirely **host-side** — the ceiling returned at redemption is a UX hint for the controller, never the security boundary.

## See also

- Global host access settings: [config.toml → `[security]`](/config/config-toml#security-security).
- The signaling-layer view of restricted sessions: [Signaling Authentication](/security/signaling-auth).
