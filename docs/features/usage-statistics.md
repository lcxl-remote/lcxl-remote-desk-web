# Usage Statistics & Retention

The portable/signal server records **TURN relay traffic** (per device) and **AI gateway token usage** (per model) into local **hourly rollups** in its SQLite database. This is collect-only telemetry for capacity and cost visibility — the single-node server does no billing.

## Usage pages & query range

The **Usage** section exposes two views — **TURN Usage** (by device) and **AI Token Usage** (by model) — each with a **time-range selector** at the top:

- **Presets**: Last 24h / Last 7 days / Last 30 days, or **Custom** start/end bounds.
- **Effective range**: the backend clamps the requested range to the configured retention and "now", and echoes the actual queried range beneath the chart.
- **Day aggregation (UTC)**: a range wider than **14 days** is automatically aggregated by **UTC calendar day** to bound the query cost; day boundaries are fixed at UTC-0 regardless of local timezone.
- **Independent of whether TURN is running**: both views read stored rollups, so past traffic remains queryable when TURN is unconfigured, failed to start, or switched off. Every mode with the local SQLite database (`default` / `signaling` / `service-daemon`) serves these pages; a pure `desk-server` has no such database and therefore no such pages.

## Retention config

The **Usage → Data Retention** page sets how many days of each rollup are kept:

- **TURN traffic retention** and **AI token usage retention**, each independent, in the range `[1, 10000]` days, **default 30 days**.
- A background cleanup loop deletes rollup rows older than the configured window. Because the portable server runs as a **single node** and does no billing, cleanup simply deletes by age — there is no billing-safety window to preserve.
- The config is a single local row saved last-writer-wins (no revision), taking effect on the next cleanup tick and query.

## Related

- [config.toml Reference](/config/config-toml)
- [Startup Modes](/guide/startup-modes)
