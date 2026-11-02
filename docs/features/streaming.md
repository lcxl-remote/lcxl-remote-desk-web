# Remote Control & Streaming

LCXL Remote Desk streams the remote screen and audio over **WebRTC** for ultra-low latency, with full keyboard and mouse control.

## Video Encoding

The capture engine supports software and hardware encoding across multiple codecs:

| Codec | Backend |
|---|---|
| H.264 | X264 / OpenH264 |
| VP8 / VP9 | libvpx |
| AV1 | SVT-AV1 |

Capture backends: Windows (**DXGI / WGC**), Linux (**X11 / Wayland portal + PipeWire**). Linux backends are filtered by the active desktop session: native X11 advertises X11, while Wayland advertises only the ScreenCast portal so XWayland is not mistaken for full-desktop capture.

The controller resolves the capture backend, display, encoder, frame rate, quality, cursor and dirty-rectangle choices for each remote-desktop connection. These choices no longer overwrite the host's configuration file. A submitted change takes effect without reloading the page: the host either applies it live, rebuilds only the affected media pipeline, or automatically establishes a replacement WebRTC session when the wire codec changes. The dialog reports the actual effect. If the controller held remote control before a replacement session, it requests control again after the new PeerConnection is connected; authorization is never copied across session epochs, so an ask policy still prompts the host again.

Before replacing a PeerConnection, the controller releases held keys and mouse buttons. Clipboard and whiteboard interaction pause until control is accepted again, and an enabled controller microphone is attached to the replacement audio transceiver without asking for capture permission again. Host-system audio is interrupted and restarted with the new media session. An attached virtual display stays attached across the handoff, but exclusive mode exits while control is unapproved so local approval UI remains visible; it may enter again after the fresh control grant.

When the encoder is set to **Auto**, the host selects the first installed encoder whose declared limits accept the selected display resolution and whose codec the controller can decode. The order is stable and runtime-probe-only encoders are not selected automatically. When an encoder is selected explicitly, that choice is strict: the host does not silently substitute another implementation or codec.

### Resolution changes and encoder limits

If the selected encoder cannot accept the current capture dimensions, the host stops before encoding instead of producing an empty video or repeatedly retrying every frame. The browser shows the current resolution, compatible encoders when known, and actions to choose another encoder or retry after changing the host display mode. A mid-session resolution change uses the same check; returning to a supported mode requires **Retry video** unless the platform reports the mode transition automatically.

No CPU-side frame scaling is performed. This avoids the high memory-bandwidth cost of scaling 4K BGRA frames and keeps the captured desktop geometry exact.

## Audio

System audio is captured and encoded with **Opus** (libopus). The Web controller explicitly requests stereo Opus reception; audible channel separation still depends on the captured source and the controller's output device. Capture backends: Windows (**WASAPI**), Linux (**ALSA / PipeWire**).

On a capable desktop host, a new browser preference starts with **Capture system audio** enabled. This is only the controller's request: the host has an independent **System audio capture** permission with Allow, Prompt and Deny behavior, and a temporary support grant may further restrict it. Denial or timeout leaves video, input and microphone uplink available while host-system audio remains off. Android hosts currently report system-audio capture as unsupported.

The controller shows the actual audio state (starting, active, denied, failed or off). The host's existing remote-access status card adds a system-audio badge only while audio is really passing its daemon output gate. Audio is WebRTC media and is not recorded or persisted by this feature. Host-system audio and controller-microphone uplink are separate features.

## Browser preferences

For signed-in owner access, the Web controller saves device settings in this browser under the controller account plus target device. Personal and organization views therefore reuse the same preference for the same account and device. The two adaptive encoder toggles are saved once per controller account. Preferences do not sync to another browser and are removed when site data is cleared. Temporary support-code/grant sessions never read or write the owner's saved preferences.

## Input

Mouse and keyboard input are injected on the controlled device over a dedicated data channel. In service-daemon mode, injection runs inside the user's desktop session — see [Startup Modes](/guide/startup-modes).

## Browser fullscreen

The toolbar's **Fullscreen** button makes the entire remote-desktop viewing area fullscreen without requiring remote-control approval. Browsers without the element fullscreen API use page maximization, keeping the toolbar and **Exit Fullscreen** available; browser address bars and system UI may remain visible. If a supported API is disabled or rejects entry or exit, the page reports the error. Embedded pages also need fullscreen permission from their containing page.

A docked toolbar stays visible and reserves its own space in fullscreen. Use **Exit Fullscreen** to return. During page maximization, Escape keeps its existing remote-key or open-menu behavior; use the toolbar to leave the maximized view.

## Toolbar layout

Drag the toolbar handle with a mouse or one finger. Near the top, bottom, left or right edge of the viewing area, a highlighted strip previews docking; release to dock. The picture fits the remaining area without stretching. Top and bottom toolbars run horizontally; side toolbars run vertically. Drag inward and release to float again, or drag along an edge to reposition the toolbar.

Docked controls stay expanded. When space is limited, scroll the button list along its direction; a mouse wheel also scrolls horizontal controls. Arrows at the list edges show where more buttons are available. Click an arrow to scroll in that direction. The handle remains outside the list. Right-click the handle, press Shift+F10 while it is focused, or hold it on a touch screen to choose a position directly. Local layout and fullscreen/leave actions remain available while reconnecting.

This browser remembers the toolbar position and the floating toolbar's explicit expanded/collapsed choice on this device. All accounts and remote devices on the same site reuse the latest saved layout; existing open windows keep their own positions. Refreshing or reopening restores it within the current screen and safe area. This layout preference is separate from account/device connection settings, including in temporary support sessions. Clearing site data resets it; it does not sync to another browser or device.


## Prepare the controlled host

The controlled host's home page shows the local actions required before it can accept a desktop session. Windows keeps the **Install service** action there; macOS reports Screen Recording and Accessibility separately and can open their system permission prompts from the same area.

On a logged-in Wayland desktop, click **Enable Wayland remote access** locally before connecting from another device. The requested scope follows the host's input mode:

- `none` and `uinput` request screen sharing only;
- `portal` and Wayland `auto` request screen sharing plus keyboard and pointer control.

The Portal picker is owned by the local desktop. A remote controller cannot open or accept it. Until the required screen/input components are ready, a remote request fails immediately with an instruction to prepare the host; it does not wait for the picker or enter WebRTC/ICE retry.

The host keeps the selected Portal session alive between remote connections. Where the desktop supports persistent restore and the packaged application has a stable identity, the next application launch attempts to restore it; otherwise the home page states that authorization lasts only for the current run. Revoking permission, restarting the Portal backend, or losing the selected source returns the host to **Needs authorization**.

The long-lived session keeps the monitor chosen in the Portal picker. Both **Enable input control** and **Change shared screen / Reauthorize** create a replacement Portal session, reopen the local system prompt, and stop active media/input pipelines first. Reconnect after authorization. No remote connection can trigger either action or upgrade screen-only authorization implicitly.

Wayland support here covers an already logged-in graphical user session. It does not cover GDM, a login screen, a headless host, or unattended operation without a graphical user session.

## Monitor Selection

`desk.video_device_name` selects which monitor to capture (e.g. `\\.\DISPLAY1` on Windows). On first connection, the browser automatically selects the first capture mode that has a usable display and then prefers the display at the desktop origin. If a saved mode or display is no longer available, it is corrected to that usable default and the dialog explains the change.

### Adaptive resolution

The configuration dialog has one **Adaptive resolution** switch for the selected display. With a Windows virtual display, the host can match the browser's viewport directly. With a supported Windows or macOS physical display, the host chooses the closest suitable mode from that monitor's existing modes. A physical mode change also affects the local screen, so each physical target starts with the switch off in a new browser session; you must turn it on. The virtual display keeps its existing browser preference.

If no better physical mode is needed or available, the current resolution is kept and the status message says **No change needed**.

Automatic changes run only while exactly one remote desktop connection exists. Other connections can still view and control the desktop. Turning the switch off stops future changes; **Restore original resolution** is available in the configuration dialog. The host also attempts restoration when the last connection leaves. If someone changes the mode locally, automatic adjustment stops and does not override that choice. Physical mode adjustment does not require the virtual display or its driver. Linux, mirrored displays, and displays whose modes cannot be safely enumerated are currently unavailable.

## Tuning Tips

- Lower `video_fps` (e.g. 30) to reduce CPU and bandwidth.
- Raise `video_quality` number for smaller bitrate, lower for better picture.
- Enable `enable_dirty_rect` for largely static screens.

## Mobile touch and local zoom

Mobile Web, iOS and Android offer two modes, with Direct touch selected initially. Open **Touch and zoom** in the toolbar to change modes or read the gesture help.

The Web toolbar shows Touch and zoom, Zoom in, Zoom out and Fit screen when touch is the primary pointing input. Desktop browsers using a mouse hide this group, including in narrow windows, while keeping Fullscreen and the other general controls. Switching to mouse input closes the touch menu and restores the local view to Fit screen.

The toolbar hides its scrollbar to keep its height stable while expanding and collapsing. When buttons exceed the available space, scrolling along the toolbar and keyboard focus still let you reach the remaining controls.

| Action | Direct touch | Touchpad |
| --- | --- | --- |
| Tap | Click at your finger | Click at the local cursor |
| One-finger swipe | Hold the left button and drag; lift to release | Move the cursor relatively |
| Hold still | One right click at your finger | One right click at the cursor |
| Arm Drag, then swipe | Direct touch already supports dragging | Press at the original cursor, move, then lift to release |

Each tap sends one click; two taps still double-click remotely. A brief ring marks a click, a double ring marks the second tap, and a dashed double ring marks a right click. These are local operation hints.

Pinch to zoom a region, or use Zoom in, Zoom out and Fit screen. The maximum follows the frame and viewport sizes, capped at 16×. Local zoom does not change the remote display resolution. When zoomed, two fingers pan the view by default; switch to scroll the remote desktop. Viewers can also zoom and pan locally without control.

The touchpad arrow is drawn immediately by the controller. Manual panning can move it outside the view; unseen targets cannot be clicked. Locate cursor moves only the local view back to it. A remote system pointer embedded in the video may remain visible as a second pointer.

Releasing control, ending the session, or detecting a disconnected connection or closed input channel clears held keys and mouse buttons on the client and host. Existing connection-detection delay still applies. Handling a physical key or finger held through reconnection is outside this change.
