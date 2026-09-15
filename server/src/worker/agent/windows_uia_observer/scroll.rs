//! Discrete semantic scrolling on the retained UIA element; no input fallback.
use super::{action_failure, unsupported};
use desk_agent_protocol::AgentError;
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, IUIAutomationScrollPattern, ScrollAmount, ScrollAmount_LargeDecrement,
    ScrollAmount_LargeIncrement, ScrollAmount_NoAmount, ScrollAmount_SmallDecrement,
    ScrollAmount_SmallIncrement, UIA_ScrollPatternId,
};

fn amount(value: i32) -> Result<ScrollAmount, AgentError> {
    match value {
        -2 => Ok(ScrollAmount_LargeDecrement),
        -1 => Ok(ScrollAmount_SmallDecrement),
        0 => Ok(ScrollAmount_NoAmount),
        1 => Ok(ScrollAmount_SmallIncrement),
        2 => Ok(ScrollAmount_LargeIncrement),
        _ => Err(unsupported(
            "semantic scroll amounts must be discrete values from -2 to 2, not pixels",
        )),
    }
}

fn pattern(element: &IUIAutomationElement) -> Result<IUIAutomationScrollPattern, AgentError> {
    unsafe { element.GetCurrentPatternAs(UIA_ScrollPatternId) }
        .map_err(|_| unsupported("the UI Automation target does not support semantic scrolling"))
}

fn axes(pattern: &IUIAutomationScrollPattern) -> Result<(bool, bool), AgentError> {
    unsafe {
        Ok((
            pattern
                .CurrentHorizontallyScrollable()
                .map_err(|_| action_failure("cannot read the horizontal scroll capability"))?
                .as_bool(),
            pattern
                .CurrentVerticallyScrollable()
                .map_err(|_| action_failure("cannot read the vertical scroll capability"))?
                .as_bool(),
        ))
    }
}

fn validate_axes(horizontal: i32, vertical: i32, axes: (bool, bool)) -> Result<(), AgentError> {
    amount(horizontal)?;
    amount(vertical)?;
    if (horizontal == 0 && vertical == 0)
        || (horizontal != 0 && !axes.0)
        || (vertical != 0 && !axes.1)
    {
        return Err(unsupported(
            "semantic scroll requires at least one supported, nonzero axis",
        ));
    }
    Ok(())
}

pub(super) fn available(element: &IUIAutomationElement) -> bool {
    pattern(element)
        .and_then(|pattern| axes(&pattern))
        .is_ok_and(|(h, v)| h || v)
}

pub(super) fn validate(
    element: &IUIAutomationElement,
    horizontal: i32,
    vertical: i32,
) -> Result<(), AgentError> {
    validate_axes(horizontal, vertical, axes(&pattern(element)?)?)
}

pub(super) fn apply(
    element: &IUIAutomationElement,
    horizontal: i32,
    vertical: i32,
) -> Result<(), AgentError> {
    let pattern = pattern(element)?;
    validate_axes(horizontal, vertical, axes(&pattern)?)?;
    let horizontal = amount(horizontal)?;
    let vertical = amount(vertical)?;
    super::super::native_ui_identity::check_mutation()?;
    unsafe { pattern.Scroll(horizontal, vertical) }
        .map_err(|_| action_failure("the UI Automation semantic scroll was rejected; inspect the current state before replanning"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discrete_amounts_preserve_direction_and_reject_pixel_distances() {
        for (input, expected) in [
            (-2, ScrollAmount_LargeDecrement),
            (-1, ScrollAmount_SmallDecrement),
            (0, ScrollAmount_NoAmount),
            (1, ScrollAmount_SmallIncrement),
            (2, ScrollAmount_LargeIncrement),
        ] {
            assert_eq!(amount(input).unwrap(), expected);
        }
        for input in [i32::MIN, -300, -3, 3, 300, i32::MAX] {
            assert!(amount(input).is_err());
        }
    }
    #[test]
    fn unsupported_axes_and_empty_actions_fail_before_dispatch() {
        for horizontal in -2..=2 {
            for vertical in -2..=2 {
                for axes in [(false, false), (false, true), (true, false), (true, true)] {
                    assert_eq!(
                        validate_axes(horizontal, vertical, axes).is_ok(),
                        (horizontal != 0 || vertical != 0)
                            && (horizontal == 0 || axes.0)
                            && (vertical == 0 || axes.1)
                    );
                }
            }
        }
    }
}
