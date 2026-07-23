# Disconnect and Lock Remote Access

The host status card provides two different local safety actions:

- **Disconnect this session** ends the selected connection's desktop, control,
  terminal, file, transfer, and execution activity. It does not prevent that
  user from connecting again.
- **Disconnect all and lock remote access** closes the host admission gate first,
  ends all current activity, and persists the lock across daemon, worker, and UI
  restarts. New owner, organization, device-code, support-code, and grant access
  is rejected until a local user unlocks the host.

The lock is an emergency stop, not a first-run policy. A new installation and a
missing state file start unlocked. An existing but unreadable or invalid state
file is shown as **Recovery locked** and is treated as locked.
Recovery first reconciles the authoritative lock round with the configured
central service; it cannot guess a lost lock ID and unlock directly.

## Local authentication

Unlock is not available through HTTP, signaling, manager settings, MCP, or a
remote terminal request. The Tauri shell starts an OS-native elevation prompt;
Windows uses UAC, Linux uses polkit/PAM through `pkexec`, and macOS uses the
administrator authentication dialog. The elevated installed helper then obtains
and consumes a short-lived, action- and version-bound challenge over the local
named pipe or Unix socket. Cancellation, an untrusted executable, a stale
version, an expired challenge, or replay leaves the host locked.

Headless hosts use the same native channel:

```text
lcxl-remote-desk-server access status
lcxl-remote-desk-server access lock
lcxl-remote-desk-server access unlock
lcxl-remote-desk-server access disconnect <connection-id>
```

Run `lock` from an interactive local terminal. Run `unlock` from an elevated
terminal (`Run as administrator`, `sudo`, or the platform equivalent). `status`
can read the durable state without modifying it when the daemon is offline;
other commands require the running daemon.

## Durability and central defense

The daemon stores the local security fact separately from ordinary settings and
uses an atomic durable replace. The local gate is authoritative. If a write
fails, current sessions are still stopped and the running process stays locked,
but the card warns that the lock is memory-only and may not survive a restart;
retry before rebooting.

When a configured central service is reachable, the host also mirrors the lock
there. The central service rejects new admissions, advances the authorization
generation once per lock round, and attempts to close the active browser peer.
The local lock succeeds even while the central service is offline and retries
the durable mirror after reconnecting. Local OS authentication is sufficient
to unlock: the host commits and applies its local unlocked state without
waiting for any central response. Central synchronization remains a durable
background outbox. If central reports a lock fence unknown to a recovered local
state, the host learns that fence and retries the central unlock without closing
the local gate again. Unlock never restores old grants,
support codes, approvals, or sessions. The persistent device-code string is not
rotated, but any grant minted before the lock remains invalid.

## Credential follow-up

Locking stops this host from accepting remote work; it does not prove that an
account password, browser session, API token, or copied data is safe. Rotate or
revoke the credential that may have leaked. Exiting the Tauri UI does not unlock
the daemon; an explicit native confirmation explains this when remote access is
active or locked.
