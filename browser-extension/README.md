# LCXL Browser Assistant extension

This Manifest V3 extension is the default controlled-edge Browser Provider for LCXL. It connects only to the device-local authenticated bridge, exposes a closed typed action set, and never exposes arbitrary script execution, cookies, storage, history, network logs, or raw DOM access to the model.

For development, load this directory as an unpacked extension in `chrome://extensions`. Pairing is a one-time device-local action. Gmail and Slack origins are built in; other HTTP or HTTPS origins require an explicit Chrome host-permission grant from the extension popup. Choose **Allow current site** for one site or **Allow all HTTP and HTTPS websites** for a single broad grant. The latter is optional, is requested only on click, and does not bypass AI action authorization. Manage or revoke site access in Chrome extension settings.

The Chrome extension is the only supported browser adapter. There is no external MCP browser process or development adapter.

After navigation, `browser_take_snapshot` can refresh an already known tab within its approved origin and account. It returns new page and element references; writes and element waits still require an exact current observation. Cross-origin/account changes require fresh authorization. A lost mutation response is never replayed: read the current page to recover, preserving an unknown outcome until independently verified.
