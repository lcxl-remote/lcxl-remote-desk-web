//! GNOME output coordinates and typed actions become bounded Portal batches.
use desk_agent_protocol::computer_use::{
    RawInputKey, RawInputMouseButton, RawInputStep, WaylandOutputInputAction,
};
use desk_wayland_portal::{PortalError, PortalInputEvent};
mod execution;
pub(crate) use execution::execute;

fn invalid() -> PortalError {
    PortalError::Backend("Wayland output geometry or input action is invalid".into())
}

/// The caller supplies dimensions from the original retained stream, not tool
/// input. Stream orientation and screenshot identity must be verified before
/// using this mapping; this converter cannot establish those facts itself.
pub(crate) fn events(
    action: &WaylandOutputInputAction,
    logical_size: (i32, i32),
) -> Result<Vec<PortalInputEvent>, PortalError> {
    action.validate().map_err(|_| invalid())?;
    let (width, height) = logical_size;
    if width <= 0 || height <= 0 || width > 32_768 || height > 32_768 {
        return Err(invalid());
    }
    let mut output = Vec::new();
    match &action.step {
        RawInputStep::Click { x, y, button } => {
            // Aim at the center of the observed pixel in stream-local logical
            // coordinates. Global desktop offsets are not Portal coordinates.
            output.push(PortalInputEvent::PointerMotionAbsolute {
                x: (f64::from(*x) + 0.5) * f64::from(width) / f64::from(action.screen.width),
                y: (f64::from(*y) + 0.5) * f64::from(height) / f64::from(action.screen.height),
            });
            let button = match button {
                RawInputMouseButton::Primary => 0x110,
                RawInputMouseButton::Secondary => 0x111,
            };
            for state in [1, 0] {
                output.push(PortalInputEvent::PointerButton { button, state });
            }
        }
        RawInputStep::KeyPress { key } => push_symbol(&mut output, navigation_symbol(*key)),
        RawInputStep::TypeText { text } => {
            for character in text.chars() {
                // Standard keysym encoding: printable Latin-1 is direct;
                // other Unicode scalars occupy the 0x01000000 namespace.
                let scalar = u32::from(character);
                let keysym = if scalar <= 0xff {
                    scalar
                } else {
                    0x01000000 | scalar
                };
                push_symbol(&mut output, keysym as i32);
            }
        }
        RawInputStep::Scroll {
            horizontal,
            vertical,
        } => {
            output.push(PortalInputEvent::PointerAxis {
                delta_x: f64::from(*horizontal),
                delta_y: f64::from(*vertical),
            });
        }
    }
    Ok(output)
}

fn push_symbol(events: &mut Vec<PortalInputEvent>, keysym: i32) {
    for state in [1, 0] {
        events.push(PortalInputEvent::KeyboardKeysym { keysym, state });
    }
}

fn navigation_symbol(key: RawInputKey) -> i32 {
    match key {
        RawInputKey::Enter => 0xff0d,
        RawInputKey::Tab => 0xff09,
        RawInputKey::Escape => 0xff1b,
        RawInputKey::Backspace => 0xff08,
        RawInputKey::Delete => 0xffff,
        RawInputKey::Space => 0x20,
        RawInputKey::ArrowUp => 0xff52,
        RawInputKey::ArrowDown => 0xff54,
        RawInputKey::ArrowLeft => 0xff51,
        RawInputKey::ArrowRight => 0xff53,
        RawInputKey::Home => 0xff50,
        RawInputKey::End => 0xff57,
        RawInputKey::PageUp => 0xff55,
        RawInputKey::PageDown => 0xff56,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{
        ScreenFrameFreshness,
        computer_use::{OutputFrameBinding, RawInputScreenContext},
    };

    fn action(step: RawInputStep) -> WaylandOutputInputAction {
        WaylandOutputInputAction {
            screen: RawInputScreenContext {
                display: "output".into(),
                width: 200,
                height: 100,
                dpi_x: 96,
                dpi_y: 96,
            },
            frame: OutputFrameBinding {
                stream_generation: 1,
                observation_id: "frame".into(),
                received_at_unix_ms: 1,
                freshness: ScreenFrameFreshness::Fresh,
            },
            step,
        }
    }

    #[test]
    fn click_scales_pixel_centers_without_global_offsets() {
        let events = events(
            &action(RawInputStep::Click {
                x: 199,
                y: 99,
                button: RawInputMouseButton::Secondary,
            }),
            (100, 50),
        )
        .unwrap();
        let PortalInputEvent::PointerMotionAbsolute { x, y } = events[0] else {
            panic!("motion first")
        };
        assert_eq!((x, y), (99.75, 49.75));
        assert!(matches!(
            events[1],
            PortalInputEvent::PointerButton {
                button: 0x111,
                state: 1
            }
        ));
        assert!(matches!(
            events[2],
            PortalInputEvent::PointerButton {
                button: 0x111,
                state: 0
            }
        ));
    }

    #[test]
    fn text_is_bounded_and_every_symbol_is_released() {
        let sequence = events(
            &action(RawInputStep::TypeText {
                text: "A中".into()
            }),
            (200, 100),
        )
        .unwrap();
        assert_eq!(sequence.len(), 4);
        assert!(matches!(
            sequence[0],
            PortalInputEvent::KeyboardKeysym {
                keysym: 0x41,
                state: 1
            }
        ));
        assert!(matches!(
            sequence[3],
            PortalInputEvent::KeyboardKeysym {
                keysym: 0x01004e2d,
                state: 0
            }
        ));
        assert!(
            events(
                &action(RawInputStep::TypeText {
                    text: "a".repeat(65)
                }),
                (200, 100)
            )
            .is_err()
        );
    }
}
