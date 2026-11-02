# Terminal, Files & Clipboard

Beyond screen control, LCXL Remote Desk combines WebRTC data-channel features with a separately authenticated remote terminal.

::: info Under a shared access code
When you connect by redeeming an [access code](/guide/access-codes) (a device or support code) rather than as the device owner, the terminal, file and clipboard capabilities below are subject to that code's capability ceiling, the host's global access settings, and live approval — not automatically available.
:::

## Remote Terminal

A built-in [xterm.js](https://xtermjs.org/) terminal supports full shell interactions over its own authenticated WebSocket, separate from the WebRTC data channels. Use it for command-line work without leaving the browser session.

The shell selector shows each discovered command once, even when PATH repeats a directory. Commands with different executable paths or arguments remain separate, so distinct Python installations and virtual environments are still selectable.

The terminal displays an LCXL Remote Desk welcome banner while connecting. After
the host confirms that the terminal session has started, closing or reloading the
page triggers the browser's leave confirmation because disconnecting ends that
remote shell and any processes running inside it. The shell-selection page and a
terminal that has already closed do not trigger this prompt.

The Terminal AI Assistant side panel can be widened or narrowed by dragging its left
edge. Its conversation follows new streaming text only while the reader remains
at the bottom; after the reader scrolls up, a down-arrow button returns to the
latest content instead of forcibly moving the current view.

Assistant renders both streaming and completed assistant prose as GitHub-flavored
Markdown, including headings, lists, code, and tables. Raw HTML is ignored and
model-supplied image URLs are not loaded.

## File Management

Manage files on the remote device directly from the browser.

The file manager initially opens the home directory of the user in the selected
remote desktop session. If that directory cannot be resolved or read, it falls
back to the Windows drive list or the filesystem root on other platforms.
The house icon (**User home**) opens the user directory again. The separate
disk icon (**My Computer**) returns directly to the root entry; refreshing or changing pages
keeps the current directory. Android, iOS, and the Manager console behave the same way.

- **Upload** files to the remote machine.
- **Download** files back to the controller.
- **Delete** files, with a **recycle-bin** mechanism for recoverable deletions.

### Drop files onto the remote desktop

On a Windows, Linux, or macOS host, drag ordinary files onto the remote picture and release to upload them to that session user's **Downloads** folder. You can drop several files; they upload one at a time. Folders are not supported. Android hosts do not currently support file transfer.

The upper-right upload panel shows each file's state and the host-confirmed save location. Existing names in Downloads are preserved: a new file receives a name such as `report (1).txt`. Collapse the panel to keep using the desktop; uploads continue in the background. In narrow or short views it starts collapsed, and the expanded list scrolls.

File transfer has its own permission. If approval is required, **Waiting for remote approval** remains until the owner answers or the host's configured approval period ends. Regular waiting updates keep this distinct from an unresponsive connection; 30 seconds without a response or progress ends the transfer.

**Waiting for remote confirmation** means the file has been sent and may still be in transit or being saved. Cancel is available before this stage. Uploads, including empty files, show success only after the host confirms saving. If the connection or final confirmation is lost, check the remote folder before retrying: an unconfirmed result may already have been saved.

Downloads must be on a filesystem that supports publishing a new file without replacing an existing name. Some network or FUSE mounts lack this operation; uploads there report an error. Choose a supported Downloads location or upload to an explicit path in the file manager.

### Uploading to a selected folder

The file manager uploads to the selected remote folder and replaces an existing file of the same name after the whole upload is received and saved. Two simultaneous uploads to the same path are refused rather than replacing each other's results. Android and iOS controllers also wait for the host's save confirmation before reporting success.

During upload, a temporary `.<name>.<id>.part` file sits beside the destination; its dot prefix does not hide it on Windows. Cancelling before submission or a failure known to occur before publication removes it and preserves the existing file. An unconfirmed publication can leave a temporary or final file for inspection.

### When file transfers are unavailable

Browsing and deleting travel over the signaling connection, while the file bytes
themselves need a WebRTC data channel. The two are established independently, so
a device the browser cannot reach over WebRTC — most often because the TURN relay
is unreachable — still lists its files: only uploads and downloads are withdrawn,
and a banner says why.

The banner also carries the connection diagnostics behind that verdict: which ICE
servers the server advertised, how many candidates of each type the browser
managed to gather, and where negotiation stopped. A relay server in the list with
**zero relay candidates** gathered means the browser cannot reach the relay, which
is worth passing on to whoever operates it — the panel's copy button produces
exactly that summary. TURN credentials are never part of it.

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

When requested by the controller and allowed by the host's independent system-audio permission, remote playback is captured and sent live to the controller as Opus. It is separate from controller-microphone uplink and is not recorded or persisted by this feature. See [Remote Control & Streaming](/features/streaming#audio).

Edit completion output in Settings / Terminal completion. The default is 512 and each request uses the smaller of this value and the model runtime limit. A thinking budget must be smaller than the effective output limit; incompatible settings produce an explicit error. Configuration is saved in config.toml.
