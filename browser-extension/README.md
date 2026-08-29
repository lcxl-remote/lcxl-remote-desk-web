# LCXL Browser Assistant extension

This Manifest V3 extension is the default controlled-edge Browser Provider for LCXL. It connects only to the device-local authenticated bridge, exposes a closed typed action set, and never exposes arbitrary script execution, cookies, storage, history, network logs, or raw DOM access to the model.

For development, load this directory as an unpacked extension in `chrome://extensions`. Pairing is a one-time device-local action. Gmail and Slack origins are built in; any other HTTPS origin requires an explicit Chrome host-permission grant from the extension popup.

Chrome DevTools MCP remains a separate, default-disabled development adapter and is not an automatic fallback for this extension.
