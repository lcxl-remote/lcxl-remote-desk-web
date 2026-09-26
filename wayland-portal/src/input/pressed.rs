//! Track only keys/buttons owned by one serialized input producer.
use super::PortalInputEvent;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    Key(i32),
    Symbol(i32),
    Button(i32),
}

fn transition(event: &PortalInputEvent) -> Option<(Held, u32)> {
    match *event {
        PortalInputEvent::KeyboardKeycode { keycode, state } => Some((Held::Key(keycode), state)),
        PortalInputEvent::KeyboardKeysym { keysym, state } => Some((Held::Symbol(keysym), state)),
        PortalInputEvent::PointerButton { button, state } => Some((Held::Button(button), state)),
        _ => None,
    }
}

#[derive(Default)]
pub(super) struct Pressed(Vec<Held>);

impl Pressed {
    pub(super) fn before(&mut self, event: &PortalInputEvent) {
        if let Some((held, 1)) = transition(event)
            && !self.0.contains(&held)
        {
            self.0.push(held);
        }
    }

    pub(super) fn after(&mut self, event: &PortalInputEvent) {
        if let Some((held, 0)) = transition(event) {
            self.0.retain(|value| *value != held);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn releases(&self) -> impl Iterator<Item = PortalInputEvent> + '_ {
        self.0.iter().rev().map(|held| match *held {
            Held::Key(keycode) => PortalInputEvent::KeyboardKeycode { keycode, state: 0 },
            Held::Symbol(keysym) => PortalInputEvent::KeyboardKeysym { keysym, state: 0 },
            Held::Button(button) => PortalInputEvent::PointerButton { button, state: 0 },
        })
    }
}

pub(super) fn valid_batch(events: &[PortalInputEvent]) -> bool {
    if events.is_empty() || events.len() > 128 {
        return false;
    }
    let mut pressed = Pressed::default();
    for event in events {
        let valid = match *event {
            PortalInputEvent::PointerMotionAbsolute { x, y } => {
                x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0
            }
            PortalInputEvent::PointerAxis { delta_x, delta_y } => {
                delta_x.is_finite() && delta_y.is_finite()
            }
            PortalInputEvent::PointerButton { button, state } => button >= 0 && state <= 1,
            PortalInputEvent::KeyboardKeycode { keycode, state } => keycode >= 0 && state <= 1,
            PortalInputEvent::KeyboardKeysym { keysym, state } => keysym > 0 && state <= 1,
        };
        if !valid {
            return false;
        }
        if let Some((held, state)) = transition(event)
            && pressed.0.contains(&held) != (state == 0)
        {
            return false;
        }
        pressed.before(event);
        pressed.after(event);
    }
    pressed.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_transitions_cannot_release_another_producers_state() {
        let press = PortalInputEvent::KeyboardKeycode {
            keycode: 42,
            state: 1,
        };
        let release = PortalInputEvent::KeyboardKeycode {
            keycode: 42,
            state: 0,
        };
        assert!(!valid_batch(&[]));
        assert!(!valid_batch(&[press.clone()]));
        assert!(!valid_batch(&[release.clone()]));
        assert!(!valid_batch(&[
            press.clone(),
            press.clone(),
            release.clone()
        ]));
        assert!(valid_batch(&[press, release]));
    }

    #[test]
    fn uncertain_release_remains_owned_until_acknowledged() {
        let press = PortalInputEvent::KeyboardKeycode {
            keycode: 42,
            state: 1,
        };
        let release = PortalInputEvent::KeyboardKeycode {
            keycode: 42,
            state: 0,
        };
        let mut held = Pressed::default();
        held.before(&press);
        held.before(&release);
        assert_eq!(held.releases().count(), 1);
        held.after(&release);
        assert!(held.is_empty());
    }
}
