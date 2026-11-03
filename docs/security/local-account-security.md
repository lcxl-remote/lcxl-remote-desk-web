# Local Account and Initialization Security

The standalone server keeps authentication throttles in bounded process memory;
it does not require Redis or another external component.

## Account login

Failures are grouped by IPv4 `/32` or IPv6 `/64` by default. Twenty failures in
a fixed 15-minute window trigger an exponential temporary lock beginning at 60
seconds. A successful login clears that network identity's state. Lock state is
not shared across processes and is cleared by restart, so internet-facing
deployments should also use upstream network protection.

Password-login thresholds are fixed. The IPv6 grouping prefix and bounded
login/redemption capacity tier are configurable. Access-code redemption has its
own protection settings, described below.

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

Send the token only in the POST body. For access from another machine, terminate
TLS in front of the server and set `LRD_COOKIE_SECURE=true`; plain HTTP can expose
the token and session cookie to the network.

## Access-code redemption

Under Settings → Device Codes → Access-code protection, configure the request window, source-network attempt limit, failure window and threshold, and initial and maximum lock durations. Seconds must be whole numbers from 1 to 86400 and attempt limits from 1 to 100000. Maximum lock must not be shorter than initial lock. OSS redemption has no separate account dimension and does not display an account limit.

Default mode starts with five attempts per source network in 60 seconds, counting successes and failures. By default, 20 unavailable codes in 15 minutes trigger a 60-second lock with exponential backoff capped at one hour. The backoff switch leaves basic request limits active. Wait before retrying when told there have been too many attempts. Standalone Signaling mode does not offer code redemption.

New attempts adopt saved parameters without clearing counters or locks. Existing windows and locks keep their deadlines; new caps apply to existing counts, and new windows and subsequent failures use the new parameters.

Redemption and password login have independent state. Successful redemption or
password login does not clear redemption failures; records and locks expire
naturally. A full bounded table rejects new sources without evicting live failure
records. Capacity uses `LRD_AUTH_RATE_LIMIT_MAX_BUCKETS` (default 65536). Successful
requests consume only the short-term quota and do not create long-term failure
records. Existing connections and redeemed grants are unaffected. Throttling
returns business code `32` with HTTP 200.

## Reverse proxies

Loopback peers (`127.0.0.0/8`, `::1`) are trusted by default. Add other proxy
addresses with `LRD_TRUSTED_PROXIES`; never trust a broad LAN merely for
convenience. `X-Forwarded-For` is ignored unless the immediate peer is trusted.
IPv4-mapped IPv6 addresses are normalized before trust and rate-limit decisions.

The current server has no CORS middleware, so a normal cross-origin browser
preflight cannot send a custom XFF header through a loopback connection. Any
future CORS or Private Network Access change must re-review this assumption.
