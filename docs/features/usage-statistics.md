# Usage Statistics & Retention

The portable/signal server records **TURN relay traffic** per device into local **hourly rollups** in its signal SQLite database. The single-node server does no billing. Model requests and token usage use the independent [Model Call Metrics](/features/model-metrics) store and page.

## Usage pages & query range

**Usage → TURN Usage** has a **time-range selector**:

- **Presets**: Last 24h / Last 7 days / Last 30 days, or **Custom** start/end bounds.
- **Effective range**: the backend clamps the requested range to the configured retention and "now", and echoes the actual queried range beneath the chart.
- **Day aggregation (UTC)**: a range wider than **14 days** is automatically aggregated by **UTC calendar day** to bound the query cost; day boundaries are fixed at UTC-0 regardless of local timezone.
- **Independent of whether TURN is running**: the page reads stored rollups, so retained traffic remains queryable when TURN is unconfigured, failed to start, or switched off. Modes with the local signal database (`default` / `signaling` / `service-daemon`) serve it; a pure `desk-server` does not.

## Retention config

The **Usage → Data Retention** page configures TURN rollups and AI conversation retention independently:

- **TURN traffic retention** is in the range `[1, 10000]` days, **default 30 days**. Model call statistics retention is managed separately under **Settings → Model call statistics settings**.
- **AI conversation retention** is also in the range `[1, 10000]` days, **default 30 days**. It controls the conversation lifecycle and its existing pending-work protections, independently of content-free model observations. Increasing a window cannot restore deleted conversations or statistics.
- A background cleanup loop deletes rollup rows older than the configured window. Because the portable server runs as a **single node** and does no billing, cleanup simply deletes by age — there is no billing-safety window to preserve.
- The configuration is saved atomically to the active `config.toml` and takes effect on subsequent cleanup ticks and queries. Rebuilding the runtime database does not reset these settings.
- Personal-assistant maintenance receipts additionally require completed original maintenance and daily-usage accounting, an absent main execution session, and completion/settlement older than the conversation-retention window. Unknown usage stays retained. Receipt cleanup refunds only its stored source-proof bytes; it does not set a lifetime for long-term memory facts or delete independent history evidence.

## Related

- [config.toml Reference](/config/config-toml)
- [Startup Modes](/guide/startup-modes)
- [Model Call Metrics](/features/model-metrics)
