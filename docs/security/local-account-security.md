# Local Account and Initialization Security

The standalone server keeps authentication throttles in bounded process memory;
it does not require Redis or another external component.

## Account login

Failures are grouped by IPv4 `/32` or IPv6 `/64` by default. Twenty failures in
a fixed 15-minute window trigger an exponential temporary lock beginning at 60
seconds. A successful login clears that network identity's state. Lock state is
not shared across processes and is cleared by restart, so internet-facing
deployments should also use upstream network protection.

These policy thresholds are intentionally fixed in this release to keep OSS
deployment configuration small. Only the IPv6 grouping prefix and bounded
login/redeem capacity tier are configurable.

## Initialization token

Set `LRD_BOOTSTRAP_TOKEN` before the first initialization to prevent an
unauthorized first visitor from claiming the administrator account. Only wrong
tokens consume the shared `/api/init` and pre-initialization connection-verify
budget: 20 failures per 10 minutes. Authorized external connection probes have
a separate budget of 60 per 10 minutes and are counted only immediately before
network access.

Both security quotas fail closed when their bounded tables are full. If an
attack has stopped but first-run setup remains blocked, restart the process or
container to clear the in-memory bootstrap/probe state; persistent settings and
the environment token are unchanged. Restart is not a defense against ongoing
traffic and is not the capacity strategy for the always-on redeem endpoint.
Redeem attempts use an independent table and retain the existing limit of five
attempts per minute for each network identity.

Send the token only in the POST body. For access from another machine, terminate
TLS in front of the server and set `LRD_COOKIE_SECURE=true`; plain HTTP can expose
the token and session cookie to the network.

## Reverse proxies

Loopback peers (`127.0.0.0/8`, `::1`) are trusted by default. Add other proxy
addresses with `LRD_TRUSTED_PROXIES`; never trust a broad LAN merely for
convenience. `X-Forwarded-For` is ignored unless the immediate peer is trusted.
IPv4-mapped IPv6 addresses are normalized before trust and rate-limit decisions.

The current server has no CORS middleware, so a normal cross-origin browser
preflight cannot send a custom XFF header through a loopback connection. Any
future CORS or Private Network Access change must re-review this assumption.
