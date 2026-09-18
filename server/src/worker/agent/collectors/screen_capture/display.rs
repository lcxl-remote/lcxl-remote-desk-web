//! Display discovery and per-call selection, independent of desktop streaming settings.

use desk_agent_protocol::computer_use::ScreenCaptureDisplay;
use desk_agent_protocol::{AgentError, AgentErrorKind, ScreenCaptureParams};
use desk_capture_engine::image_capture::image_capture_factory::list_effective_image_output;
use desk_signal_facade::model::{desk_settings::DeskSettings, image_capture::DisplayInfo};

const MAX_DISPLAYS: usize = 32;

fn invalid(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

pub(crate) fn list(settings: &DeskSettings) -> Result<Vec<ScreenCaptureDisplay>, AgentError> {
    let outputs = list_effective_image_output(settings).map_err(super::capture_err)?;
    project(outputs)
}

fn project(outputs: Vec<DisplayInfo>) -> Result<Vec<ScreenCaptureDisplay>, AgentError> {
    let outputs: Vec<_> = outputs
        .into_iter()
        .filter(|d| d.attached_to_desktop)
        .collect();
    // Never truncate a list then mistake its first entry for the only display.
    if outputs.len() > MAX_DISPLAYS {
        return Err(invalid("too many attached displays to list safely"));
    }
    let mut displays = Vec::new();
    for d in outputs {
        if d.device_name.trim().is_empty() || d.device_name.len() > 512 {
            return Err(invalid(
                "capture backend returned an invalid display identifier",
            ));
        }
        if displays
            .iter()
            .any(|v: &ScreenCaptureDisplay| v.display == d.device_name)
        {
            return Err(invalid(
                "capture backend returned ambiguous display identifiers",
            ));
        }
        let rect = d.desktop_coordinates;
        let width = i64::from(rect.right) - i64::from(rect.left);
        let height = i64::from(rect.bottom) - i64::from(rect.top);
        let resolution = d.current_capture_resolution;
        displays.push(ScreenCaptureDisplay {
            display: d.device_name.clone(),
            name: d
                .display_device_name
                .unwrap_or(d.device_name)
                .chars()
                .take(128)
                .collect(),
            width: resolution
                .map(|r| r.width)
                .unwrap_or(u32::try_from(width).unwrap_or(0)),
            height: resolution
                .map(|r| r.height)
                .unwrap_or(u32::try_from(height).unwrap_or(0)),
            x: rect.left,
            y: rect.top,
        });
    }
    Ok(displays)
}

fn select<'a>(
    displays: &'a [ScreenCaptureDisplay],
    requested: Option<&str>,
) -> Result<&'a str, AgentError> {
    if let Some(requested) = requested {
        return displays.iter().find(|d| d.display == requested).map(|d| d.display.as_str())
            .ok_or_else(|| invalid("requested display is no longer available; refresh inspect_desktop_session, select a current displays[].display, and request screenshot permission again"));
    }
    match displays {
        [only] => Ok(&only.display),
        [] => Err(invalid("no attached display is available for capture")),
        _ => Err(invalid(
            "multiple displays are available; call inspect_desktop_session, choose one displays[].display, then request read_current_screen with that display; no screenshot was taken",
        )),
    }
}

pub(crate) fn resolve(
    settings: &DeskSettings,
    params: &ScreenCaptureParams,
) -> Result<DeskSettings, AgentError> {
    if params.window.is_some() {
        return Ok(settings.clone());
    }
    let displays = list(settings)?;
    let target = select(&displays, params.display.as_deref())?;
    let mut resolved = settings.clone();
    resolved.video_device_name = target.to_string();
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires a real interactive desktop; enumerates metadata only"]
    fn live_display_discovery_and_selection_without_capture() {
        let settings = DeskSettings::default();
        let displays = list(&settings).unwrap();
        assert!(!displays.is_empty());
        for display in &displays {
            println!(
                "{}: {}x{} at {},{}",
                display.display, display.width, display.height, display.x, display.y
            );
            assert_eq!(
                select(&displays, Some(&display.display)).unwrap(),
                display.display
            );
        }
        assert_eq!(select(&displays, None).is_ok(), displays.len() == 1);
        assert!(settings.video_device_name.is_empty());
    }
    fn screen(id: &str) -> ScreenCaptureDisplay {
        ScreenCaptureDisplay {
            display: id.into(),
            name: id.into(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
        }
    }
    #[test]
    fn single_display_is_automatic_but_multiple_displays_require_a_target() {
        assert_eq!(select(&[screen("one")], None).unwrap(), "one");
        assert!(select(&[screen("one"), screen("two")], None).is_err());
        assert_eq!(
            select(&[screen("one"), screen("two")], Some("two")).unwrap(),
            "two"
        );
    }
    #[test]
    fn removed_or_missing_targets_never_fall_back() {
        assert!(select(&[], None).is_err());
        assert!(select(&[screen("other")], Some("removed")).is_err());
        assert!(select(&[screen("one")], Some("")).is_err());
    }
    #[test]
    fn enumeration_is_complete_and_excludes_detached_outputs() {
        let output = |id: &str, attached| DisplayInfo {
            device_name: id.into(),
            attached_to_desktop: attached,
            ..Default::default()
        };
        assert_eq!(
            project(vec![output("one", true), output("two", false)])
                .unwrap()
                .len(),
            1
        );
        assert!(project(vec![output("same", true), output("same", true)]).is_err());
        assert!(project((0..33).map(|n| output(&n.to_string(), true)).collect()).is_err());
    }
}
