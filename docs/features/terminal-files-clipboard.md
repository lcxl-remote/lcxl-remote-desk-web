# Terminal, Files & Clipboard

Beyond screen control, LCXL Remote Desk bundles everyday productivity features over dedicated WebRTC data channels.

::: info Under a shared access code
When you connect by redeeming an [access code](/guide/access-codes) (a device or support code) rather than as the device owner, the terminal, file and clipboard capabilities below are subject to that code's capability ceiling, the host's global access settings, and live approval — not automatically available.
:::

## Remote Terminal

A built-in [xterm.js](https://xtermjs.org/) terminal supports full shell interactions over a dedicated data channel. Use it for command-line work without leaving the browser session.

The terminal displays an LCXL Remote Desk welcome banner while connecting. After
the host confirms that the terminal session has started, closing or reloading the
page triggers the browser's leave confirmation because disconnecting ends that
remote shell and any processes running inside it. The shell-selection page and a
terminal that has already closed do not trigger this prompt.

## File Management

Manage files on the remote device directly from the browser:

- **Upload** files to the remote machine.
- **Download** files back to the controller.
- **Delete** files, with a **recycle-bin** mechanism for recoverable deletions.

A transfer the host refuses — file transfer is off in its security settings, or
the owner declined the prompt — is reported as a refusal on the transfer row
rather than sitting at 0%. If the host stops responding partway through, the
transfer is abandoned after 30 seconds and can be retried.

Uploading over a file that already exists replaces it, and only ever in one
step. While the upload runs, the bytes go to a temporary `.<name>.<id>.part`
file next to the destination; the destination itself is untouched until the
whole file has arrived and reached stable storage, at which point the temporary
file takes its name. So an upload that is cancelled, runs out of room or loses
its connection costs nothing — the file that was already there is still the
file that is there, and the temporary one is removed. Two transfers uploading
to the same path at once would each replace the other's result, so the second
one is refused rather than accepted.

### Mapped network drives on a Windows service host

A host running as the Windows system service browses files from an elevated
account in a different logon session than the desktop user, and Windows scopes
mapped network drives to a logon session. Drive letters the user mapped in
Explorer are therefore missing from the drive list. Reach those locations by
their UNC path (`\\server\share`) instead, or map them for both sessions by
setting `EnableLinkedConnections` to `1` and restarting the machine.

The file manager says so on the drive-list page when the host reports itself as
a service host — it asks the host, so the note appears when browsing remotely
through a signaling server or a manager, not only when the browser and the host
are the same machine.

## Clipboard Sync

**Bidirectional** synchronization for text clipboards keeps copy/paste seamless between the controller and the remote device.

## System Audio

Remote audio playback is captured and synchronized to the controller (Opus-encoded). See [Remote Control & Streaming](/features/streaming#audio).
