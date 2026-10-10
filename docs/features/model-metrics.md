# Model call metrics

Open **Usage → Model call metrics**. Only the signed-in device owner can access this page; device-code guests cannot.

Use this page to see whether model calls succeed, which tool parameters cause problems, and how many calls are made and how long they take. **Statistics may be incomplete in exceptional situations. Use them as a reference.** A statistics outage does not affect model responses, tool permissions, execution or usage processing.

## View and filter

Choose a time range, then filter by model, feature, purpose, tool or error category. A query can cover up to 90 days.

- **Overview**: call counts, error rates, duration and trends. Click a statistics card to see matching records.
- **Models**: compare calls across models.
- **Tool inputs / references / correction**: inspect parameter checks and the changes a model makes after feedback.
- **Calls**: inspect individual results, tool input counts and Token usage. You can focus on errors or slow calls.
- **Runtime observations**: inspect assistant turns, context compression and related services.
- **Unassociated facts**: inspect records missing original call information. These cannot yet contribute to statistics and are filtered by reception time only.

**Token usage** shows input, output and cache usage with its own time range. Call quality uses call start times, while Token usage uses reporting times, so the totals can differ.

All page times, including query ranges, trends, call details and update times, use **this device’s local time zone** and display only the date and time. Changing the interface language does not change the time zone. Exported timestamps retain a standard format with a time zone so the same instant can be compared accurately.

The overview can compare two periods. Compare record counts, model settings and data completeness as well as rates. A lower error rate alone does not establish that a model or tool change caused the improvement.

## Understand the results

| Metric | Meaning |
|---|---|
| Model calls | Application requests to a model. Retries within one request are listed separately as HTTP attempts. |
| Request failure rate | Known failures divided by returned calls plus known failures. Canceled, unsent and unknown outcomes are listed separately. |
| Tool parameter errors | Whether generated parameters pass format, field, reference and pre-execution checks. A user declining permission is not a parameter error. |
| Next-input acceptance rate | Whether the next reliably linked input passes checks after error feedback. Acceptance does not mean the task was completed. |
| Execution verification rate | How many dispatched operations have a verified result. Accepted or unknown results are not counted as verified success. |
| P50 / P95 / P99 duration | Estimates of the duration distribution. About 95% of recorded durations are at or below P95. |
| Token usage | Usage reported by the model. Missing values stay unknown instead of being filled with zero. |

Rates include the number of records used in the calculation. A very small sample, such as one call, is not directly comparable with many calls. With no usable records, the page does not display `0%`.

Only some models or tools may be listed. Use an exact ID or tool name to find an unlisted item; remaining groups are combined as Other. Parameter-field rankings also reflect only retained records.

## When data is incomplete

Collection is enabled by default and updates in the background. Service failures, process exits, delayed processing or storage pressure can leave some records unsaved. Older records can also be removed because of retention settings or capacity limits.

The page shows collection status, the first collection time, pending records and known gaps. **No data does not mean no errors.** Some failures also make the exact number of lost records unknown.

Only new records collected after this feature is enabled are available. Earlier calls, old counters and logs are not imported or recalculated. Records missing original call information may update when related information arrives; this never repeats a model call or tool action.

## Statistics settings and retention

Open **Settings → Signaling server → Model call statistics settings**, or use the settings button at the top of the metrics page.

| Setting | Default | Range |
|---|---|---|
| Call details | 7 days | 1–90 days |
| Five-minute summaries | 7 days | 1–30 days |
| Hourly summaries | 90 days | 1–365 days |
| Window for delayed results | 7 days | No longer than detail and hourly retention |
| Detail / related-record limits | 100,000 each | Related-record limit must cover the detail limit |
| Records awaiting statistics | 20,000 | 1,000–1,000,000 |
| Summary records | 25,000 | 1,000–1,000,000 |
| Groups per time interval | 256 | 16–1,024 |
| Statistics storage limit | 256 MiB | 16 MiB–16 GiB |

Capacity limits can remove records before their retention time expires. Shorter retention removes older records; longer retention or a higher limit cannot restore deleted data. Actual disk usage also includes indexes and database working files and can exceed the statistics storage limit.

If another administrator changes settings, load and review the latest settings before saving to avoid overwriting their changes. TURN, conversation and billing retention remain in their respective retention settings.

## Export and privacy

The overview, model and tool tabs export statistics for the current filters. **Export current page CSV** exports only the displayed calls page and does not download subsequent pages.

Exports include filters, time ranges and data completeness. Details and CSV files contain no questions, model responses, tool parameter values, screenshots, commands, file paths or secrets. Call exports also exclude user, device, conversation and internal association identifiers. Opening a record does not grant execution permission.

## Backup and troubleshooting

Collection and retention settings are stored in `[model_metrics]` in the active `config.toml`; records and runtime watermarks are stored in the separate `model-metrics.sqlite` file. Use a consistent SQLite backup, or stop the service before copying files. Copying only the main file while the service is running can miss recent data.

Portable, signaling and system-service modes that provide local signaling support these statistics. Desk-server-only mode does not. If you connect to an external Manager, view platform statistics in Manager.

If data is temporarily unavailable, turn off automatic refresh and check database space, permissions and service logs. After a repair, check the collection state and latest update time. Do not clear business data to fix a separate statistics failure. A backup cannot recover records that were never saved, repeat tool actions or recalculate bills.

## Latest main-model context observation

The session panel separates the retained model-view byte budget from the latest main-model input tokens. Reported cache input is included once. Native-clearing counts are shown separately and are never subtracted again. Missing or incomplete usage stays unknown; configuration or summary changes mark an earlier observation stale. Summary calls and cumulative session usage do not replace main-model input, and no provider-window percentage is shown.

Thinking replay follows the configured explicit protocol contract. DeepSeek retains full thinking history; Anthropic retains complete signed blocks and can use native clearing where the endpoint supports it. Clearing does not reduce HTTP request bytes, so the local byte budget still applies. UI references use session-owned `s1/a1/w1/e1/o1` and browser `p1/b1` aliases. Observe again when a reference expires or an object changes.
