# Virtual Display

LCXL Remote Desk can present a **virtual display** to the controlled device, useful for headless hosts or for dedicating a screen to the remote session.

::: tip Platform support
The virtual display is based on the Windows Indirect Display Driver (**IddCx**) and requires an installed driver. It is effective only in specific startup modes; other modes reject the related signaling.
:::

## Configuration

The virtual display is controlled under `[virtual_display]` in `config.toml`:

- `enabled` — turn the virtual display on (requires an installed IddCx driver).
- `exclusive` — exclusive-mode toggle.
- `prompt_ms` — countdown prompt duration before switching.
- `adaptive_*` — adaptive-resolution parameters.

See the [config.toml Reference](/config/config-toml#virtual-display-virtual-display) for the full field list.

## Userspace Abstraction

The userspace side is implemented as a Rust crate (`desk-virtual-display`) with a trait plus a Windows IDD implementation and stubs on other platforms. Driver install/uninstall is wrapped by `desk-virtual-display-driver-ops`.
