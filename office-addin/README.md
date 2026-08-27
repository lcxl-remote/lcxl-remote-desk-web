# Lcxl Excel read bridge

This independent OSS Office add-in hosts the production read-only Excel semantic bridge. It is not part of an npm workspace and it does not import the isolated proof-of-concept under `pocs/`.

The exposed protocol contains one action: `inspect_selection`. It reads at most 16 selected cells and returns only the address, dimensions, formulas, scalar values, and number formats. There is no write, free-form JavaScript, PowerPoint, COM, UI Automation, or raw-input action in this module.

Microsoft requires `ReadWriteDocument` for Excel's application-specific JavaScript APIs, including read-only calls. The manifest therefore requests that host permission even though both the task pane action allowlist and the broker protocol are read-only. `npm test` includes a source-level regression gate that rejects known write surfaces.

## Local development

The broker binds only `127.0.0.1` and requires a localhost certificate trusted by Office. The certificate, private key, admin token file, and optional pairing-offer file must remain owner-accessible local files.

```powershell
$env:OFFICE_BRIDGE_CERT = 'C:\absolute\path\localhost.crt'
$env:OFFICE_BRIDGE_KEY = 'C:\absolute\path\localhost.key'
$env:OFFICE_BRIDGE_ADMIN_TOKEN_FILE = 'C:\absolute\private\office-admin.token'
$env:OFFICE_BRIDGE_PAIRING_FILE = 'C:\absolute\private\office-pairing.json'
npm run broker
```

The trusted local host reads the pairing offer and shows its six-digit code to the device owner. The owner explicitly opens the task pane in the target saved workbook and enters that code. The offer is single-use and expires after two minutes by default. The session is bound to the current document URL hash, expires after 15 seconds without task-pane polling, and is revoked when the task pane observes a document change.

For development sideloading, use Microsoft Office add-in debugging tooling with `manifest.xml`. Development sideloading is not a distribution mechanism. Production personal distribution still requires Microsoft Marketplace packaging and review; organization distribution can use Microsoft 365 integrated apps.

## Verification

```powershell
npm run build
```

The build uses only Node.js built-ins and does not install dependencies. It checks JavaScript syntax, pairing/session boundaries, strict action/result schemas, queue correlation, size limits, and absence of write action code.
