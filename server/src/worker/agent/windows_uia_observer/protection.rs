//! Screenshot safety is independent of model-facing UI projection budgets.
use super::*;
use std::collections::HashSet;
use windows::Win32::Foundation::HWND;
use windows::core::Interface;

fn incomplete(detail: &'static str) -> AgentError {
    tracing::warn!(detail, "Windows screenshot safety scan incomplete");
    failure_with_kind(
        AgentErrorKind::SessionUnavailable,
        &format!(
            "screenshot safety check could not complete: {detail}; no protected content was confirmed and no screenshot was taken. Retry after the application responds."
        ),
        true,
    )
}

fn checked<T>(value: windows::core::Result<T>, field: &'static str) -> Result<T, AgentError> {
    value.map_err(|error| {
        tracing::warn!(
            field,
            hresult = error.code().0,
            "Windows screenshot safety property failed"
        );
        incomplete(field)
    })
}

// The generated wrapper converts S_OK + null (normal end of siblings) into an
// error. Inspect the native HRESULT and nullable output separately instead.
fn adjacent(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    first: bool,
) -> Result<Option<IUIAutomationElement>, AgentError> {
    let mut raw = std::ptr::null_mut();
    let status = unsafe {
        let method = if first {
            walker.vtable().GetFirstChildElement
        } else {
            walker.vtable().GetNextSiblingElement
        };
        method(walker.as_raw(), element.as_raw(), &mut raw)
    };
    let value = (!raw.is_null()).then(|| unsafe { IUIAutomationElement::from_raw(raw) });
    checked(
        status.ok(),
        if first {
            "first child unavailable"
        } else {
            "next sibling unavailable"
        },
    )?;
    Ok(value)
}

fn scan_graph<T, K: Eq + std::hash::Hash>(
    root: T,
    deadline: Instant,
    mut key: impl FnMut(&T) -> Result<K, AgentError>,
    mut inspect: impl FnMut(&T) -> Result<(bool, Vec<T>), AgentError>,
) -> Result<bool, AgentError> {
    let mut pending = vec![root];
    let mut visited = HashSet::new();
    while let Some(node) = pending.pop() {
        if Instant::now() >= deadline {
            return Err(incomplete("UIA traversal timed out"));
        }
        if !visited.insert(key(&node)?) {
            continue;
        }
        let (protected, children) = inspect(&node)?;
        if protected {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Err(incomplete("UIA traversal timed out"));
        }
        pending.extend(children);
    }
    Ok(false)
}

pub(super) fn scan(
    automation: &IUIAutomation,
    root: IUIAutomationElement,
) -> Result<bool, AgentError> {
    let walker = checked(
        unsafe { automation.RawViewWalker() },
        "raw UIA tree unavailable",
    )?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(8);
    let mut nodes = 0usize;
    let result = scan_graph(
        root,
        deadline,
        |node| runtime_key(node).map_err(|_| incomplete("UIA identity unavailable")),
        |node| {
            nodes += 1;
            let offscreen = checked(
                unsafe { node.CurrentIsOffscreen() },
                "visibility unavailable",
            )?
            .as_bool();
            if !offscreen
                && checked(
                    unsafe { node.CurrentIsPassword() },
                    "password marker unavailable",
                )?
                .as_bool()
            {
                return Ok((true, Vec::new()));
            }
            // An offscreen container can still expose visible descendants.
            let mut children = Vec::new();
            let mut sibling_ids = HashSet::new();
            let mut child = adjacent(&walker, node, true)?;
            while let Some(current) = child {
                if Instant::now() >= deadline {
                    return Err(incomplete("UIA traversal timed out"));
                }
                let id = runtime_key(&current)
                    .map_err(|_| incomplete("UIA child identity unavailable"))?;
                if !sibling_ids.insert(id) {
                    return Err(incomplete("UIA sibling cycle"));
                }
                child = adjacent(&walker, &current, false)?;
                children.push(current);
            }
            Ok((false, children))
        },
    );
    tracing::debug!(nodes, elapsed_ms = started.elapsed().as_millis() as u64, completed = result.is_ok(), protected = ?result.as_ref().ok(), "Windows screenshot safety scan");
    result
}

pub(super) fn foreground(process_id: u32, image_path: &str) -> Result<bool, AgentError> {
    let image_path = image_path.to_owned();
    super::super::native_ui_identity::run(move || {
        let before = resolve_foreground_application()?;
        if before.process_id != process_id || !path_eq(&before.image_path, &image_path) {
            return Err(incomplete("foreground application changed"));
        }
        let _com = ComGuard::initialize()?;
        let automation: IUIAutomation = checked(
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) },
            "UIA unavailable",
        )?;
        let root = checked(
            unsafe { automation.ElementFromHandle(HWND(before.window_handle as *mut _)) },
            "foreground root unavailable",
        )?;
        let result = scan(&automation, root)?;
        let after = resolve_foreground_application()?;
        if before.window_handle != after.window_handle
            || before.process_id != after.process_id
            || before.process_started_at != after.process_started_at
            || !path_eq(&before.image_path, &after.image_path)
        {
            return Err(incomplete("foreground application changed during scan"));
        }
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deep_large_trees_do_not_inherit_projection_limits() {
        let result = scan_graph(
            0usize,
            Instant::now() + Duration::from_secs(2),
            |n| Ok(*n),
            |n| Ok((false, if *n < 4096 { vec![n + 1] } else { vec![0] })),
        );
        assert_eq!(result.unwrap(), false);
    }
    #[test]
    fn protected_and_incomplete_are_different_results() {
        let deadline = Instant::now() + Duration::from_secs(2);
        assert!(scan_graph(0, deadline, |n| Ok(*n), |_| Ok((true, vec![]))).unwrap());
        let error = scan_graph(
            0,
            deadline,
            |n| Ok(*n),
            |_| Err(incomplete("password marker unavailable")),
        )
        .unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::SessionUnavailable);
        assert!(error.retryable);
        assert!(scan_graph(0, Instant::now(), |n| Ok(*n), |_| Ok((false, vec![]))).is_err());
    }
}
