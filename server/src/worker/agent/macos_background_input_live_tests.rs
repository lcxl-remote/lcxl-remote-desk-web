//! Opt-in production-adapter smoke test using only a named synthetic document.
use super::*;
use crate::worker::agent::macos_background_input as input;
use desk_agent_protocol::background_input::{
    BackgroundInputAction as Action, InputModifier, WindowInputGeometry, WindowInputPosition,
};

#[repr(C)]
#[derive(Default, Debug, PartialEq)]
struct TextRange {
    location: isize,
    length: isize,
}
fn text_range(pid: u32, path: &str, id: &str, attribute: &str) -> TextRange {
    let (path, id, attribute) = (path.to_owned(), id.to_owned(), attribute.to_owned());
    super::super::native_ui_identity::run(move || {
        let element = locate_action_target(pid, &path, &id)?;
        let value = copy_attribute(element.0, &attribute).expect("TextEdit range attribute");
        let mut range = TextRange::default();
        assert!(unsafe { AXValueGetValue(value.0, 4, (&mut range as *mut TextRange).cast()) });
        Ok(range)
    })
    .unwrap()
}

#[test]
#[ignore = "requires authorized desktop and a dedicated background TextEdit document"]
fn production_background_input_textedit() {
    let title = std::env::var("LRDM_BACKGROUND_TEST_DOCUMENT").expect("test document title");
    assert!(title.starts_with("LRDM-Background-Input-"));
    let app = running_applications()
        .unwrap()
        .into_iter()
        .find(|a| a.image_path.contains("TextEdit.app/"))
        .expect("TextEdit");
    let front = frontmost_application().unwrap().process_id;
    assert_ne!(front, app.process_id, "TextEdit must be background");
    let cursor = || {
        core_graphics::event::CGEvent::new(
            core_graphics::event_source::CGEventSource::new(
                core_graphics::event_source::CGEventSourceStateID::Private,
            )
            .unwrap(),
        )
        .unwrap()
        .location()
    };
    let initial = cursor();
    let all = collect_application(app.process_id, &app.image_path, 16, 1000, 1024 * 1024).unwrap();
    let window = all
        .nodes
        .iter()
        .find(|n| n.role.starts_with("AXWindow") && n.name.as_deref() == Some(&title))
        .expect("dedicated document window")
        .fingerprint
        .clone();
    let read = || {
        collect_application_selection(
            app.process_id,
            &app.image_path,
            16,
            1000,
            1024 * 1024,
            UiInspectScope::Content,
            Some(&window),
            None,
            false,
        )
        .unwrap()
    };
    let text = |tree: &CollectedUiTree| {
        tree.nodes
            .iter()
            .find(|n| n.role.starts_with("AXTextArea"))
            .and_then(|n| n.value.clone())
            .expect("text value")
    };
    let before = text(&read());
    let target = background_target(
        app.process_id,
        app.image_path.clone(),
        window.clone(),
        None,
        true,
    )
    .unwrap();
    input::apply(
        &target,
        &Action::TypeText {
            text: "[PRODUCTION-后台🙂]".into(),
        },
        None,
        || Ok(()),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let after = text(&read());
    assert!(!before.contains("[PRODUCTION-后台🙂]"));
    assert!(after.contains("[PRODUCTION-后台🙂]"));
    let tree = read();
    let element = tree
        .nodes
        .iter()
        .find(|n| n.role.starts_with("AXTextArea"))
        .unwrap();
    let target = background_target(
        app.process_id,
        app.image_path.clone(),
        window.clone(),
        Some(element.fingerprint.clone()),
        false,
    )
    .unwrap();
    let range = |name| text_range(app.process_id, &app.image_path, &element.fingerprint, name);
    let initial_selection = range("AXSelectedTextRange");
    for other in all.nodes.iter().filter(|n| {
        n.role.starts_with("AXWindow")
            && n.name
                .as_deref()
                .is_some_and(|name| name.starts_with("LRDM-Background-Input-") && name != title)
    }) {
        assert!(
            background_target(
                app.process_id,
                app.image_path.clone(),
                other.fingerprint.clone(),
                None,
                true
            )
            .is_err(),
            "keyboard must not enter another document"
        );
    }
    let capture = resolve_window_capture_target(app.process_id, &app.image_path, &window).unwrap();
    let shot = super::super::collectors::screen_capture::collect(
        &desk_agent_protocol::ScreenCaptureParams::default(),
        &desk_signal_facade::model::desk_settings::DeskSettings::default(),
        Some(capture),
    )
    .unwrap();
    assert!(shot.window_geometry.is_some());
    std::fs::write("/tmp/lrdm-bg-production-window.png", shot.image).unwrap();
    let point = target.element_point.unwrap();
    let geometry = WindowInputGeometry {
        width_millipoints: (target.width * 1000.0).round() as u64,
        height_millipoints: (target.height * 1000.0).round() as u64,
    };
    let position = WindowInputPosition {
        x: ((point.x - target.origin.x) / target.width * 1000.0).round() as u16,
        y: ((point.y - target.origin.y) / target.height * 1000.0).round() as u16,
    };
    let click = Action::Click {
        position: Some(position.clone()),
        element: None,
    };
    input::apply(&target, &click, Some(&geometry), || Ok(())).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let clicked = range("AXSelectedTextRange");
    assert_ne!(
        clicked, initial_selection,
        "mouse must change insertion position"
    );
    input::apply(
        &target,
        &Action::TypeText {
            text: "[MOUSE-INSERT]".into(),
        },
        None,
        || Ok(()),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(text(&read()).contains("[MOUSE-INSERT]"));
    for action in [
        Action::DoubleClick {
            position: Some(position.clone()),
            element: None,
        },
        Action::KeyPress {
            key: "ArrowRight".into(),
            modifiers: vec![InputModifier::Shift],
        },
        Action::KeyPress {
            key: "ArrowLeft".into(),
            modifiers: vec![InputModifier::Command],
        },
        Action::Scroll {
            position: Some(position),
            element: None,
            horizontal: 0,
            vertical: -600,
        },
    ] {
        let selection_before = range("AXSelectedTextRange");
        let visible_before = range("AXVisibleCharacterRange");
        input::apply(&target, &action, Some(&geometry), || Ok(())).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let selection_after = range("AXSelectedTextRange");
        match &action {
            Action::DoubleClick { .. } => {
                assert!(selection_after.length > 0, "double click selects text")
            }
            Action::KeyPress { .. } => assert_ne!(
                selection_after, selection_before,
                "modifier key must update selection"
            ),
            Action::Scroll { .. } => assert_ne!(
                range("AXVisibleCharacterRange"),
                visible_before,
                "scroll must update visible text"
            ),
            _ => unreachable!(),
        }
        println!(
            "{:?}: selection {:?} -> {:?}",
            action.kind(),
            selection_before,
            selection_after
        );
        assert_eq!(frontmost_application().unwrap().process_id, front);
        let current = cursor();
        assert_eq!((current.x, current.y), (initial.x, initial.y));
    }
    let mut stale = geometry;
    stale.width_millipoints += 1000;
    assert!(
        input::validate(&target, &click, Some(&stale))
            .unwrap_err()
            .message
            .contains("size changed")
    );
    println!(
        "Production input passed: Unicode, mouse/type, double click, modifiers, scroll dispatch; foreground/cursor unchanged; resized-window preflight rejected."
    );
}
