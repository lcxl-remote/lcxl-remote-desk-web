# Terminal, Files & Clipboard

Beyond screen control, LCXL Remote Desk bundles everyday productivity features over dedicated WebRTC data channels.

::: info Under a shared access code
When you connect by redeeming an [access code](/guide/access-codes) (a device or support code) rather than as the device owner, the terminal, file and clipboard capabilities below are subject to that code's capability ceiling, the host's global access settings, and live approval — not automatically available.
:::

## Remote Terminal

A built-in [xterm.js](https://xtermjs.org/) terminal supports full shell interactions over a dedicated data channel. Use it for command-line work without leaving the browser session.

## File Management

Manage files on the remote device directly from the browser:

- **Upload** files to the remote machine.
- **Download** files back to the controller.
- **Delete** files, with a **recycle-bin** mechanism for recoverable deletions.

A transfer the host refuses — file transfer is off in its security settings, or
the owner declined the prompt — is reported as a refusal on the transfer row
rather than sitting at 0%. If the host stops responding partway through, the
transfer is abandoned after 30 seconds and can be retried.

## Clipboard Sync

**Bidirectional** synchronization for text clipboards keeps copy/paste seamless between the controller and the remote device.

## System Audio

Remote audio playback is captured and synchronized to the controller (Opus-encoded). See [Remote Control & Streaming](/features/streaming#audio).
