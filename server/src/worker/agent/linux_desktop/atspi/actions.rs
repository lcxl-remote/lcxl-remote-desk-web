//! Each side-effecting D-Bus call rechecks the shared native mutation guard.
mod verification;
use super::*;
use desk_agent_protocol::computer_use::{UiSemanticAction, UiSemanticActionKind};
type Identity = (String, String, String, u32, u64, String, u64);

pub(crate) struct AppliedAction {
    pub changed: bool,
    pub verified: bool,
    pub summary: String,
}

async fn locate(bus: &Bus, pid: u32, executable: &str, fingerprint: &str) -> Result<Object> {
    let (desktop, bus_id, owner, recorded_pid, started, path, epoch): Identity =
        serde_json::from_str(fingerprint).map_err(error)?;
    if lifetime::epoch(&bus.id)? != epoch
        || recorded_pid != pid
        || bus.desktop.binding() != desktop
        || bus.id != bus_id
        || process(pid)? != (executable.into(), started)
    {
        return Err(error("AT-SPI target lifetime changed"));
    }
    let app = bus
        .applications()
        .await?
        .into_iter()
        .find(|app| app.pid == pid && app.object.0 == owner && app.started == started)
        .ok_or_else(|| error("AT-SPI application owner changed"))?;
    Ok((
        app.object.0,
        OwnedObjectPath::try_from(path).map_err(error)?,
    ))
}

pub(super) async fn supported(
    bus: &Bus,
    object: &Object,
    role: &str,
    states: &[u32],
) -> Result<Vec<UiSemanticActionKind>> {
    if role == "password text" || state(states, 6) || !state(states, 8) || !state(states, 25) {
        return Ok(Vec::new());
    }
    let accessible = bus.proxy(object).await?;
    let interfaces: Vec<String> = accessible.call("GetInterfaces", &()).await.map_err(error)?;
    let has = |name| interfaces.iter().any(|value| value == name);
    let mut actions = Vec::new();
    if has("org.a11y.atspi.Action") {
        let proxy = Proxy::new(
            &bus.connection,
            object.0.as_str(),
            object.1.as_str(),
            "org.a11y.atspi.Action",
        )
        .await
        .map_err(error)?;
        let native: Vec<(String, String, String)> =
            proxy.call("GetActions", &()).await.map_err(error)?;
        if native
            .iter()
            .any(|(name, _, _)| matches!(name.as_str(), "click" | "press" | "activate"))
        {
            actions.push(UiSemanticActionKind::Invoke);
            if matches!(role, "check box" | "toggle button" | "check menu item") {
                actions.push(UiSemanticActionKind::Toggle);
            }
        }
    }
    if has("org.a11y.atspi.EditableText") && state(states, 7) {
        actions.push(UiSemanticActionKind::SetValue);
    }
    if has("org.a11y.atspi.Component") && state(states, 11) {
        actions.push(UiSemanticActionKind::Focus);
    }
    if state(states, 22) {
        actions.push(UiSemanticActionKind::Select);
    }
    Ok(actions)
}

async fn validate(bus: &Bus, object: &Object, action: &UiSemanticAction) -> Result<Vec<u32>> {
    let accessible = bus.proxy(object).await?;
    let role: String = accessible.call("GetRoleName", &()).await.map_err(error)?;
    let states: Vec<u32> = accessible.call("GetState", &()).await.map_err(error)?;
    let required = match action {
        UiSemanticAction::Invoke => UiSemanticActionKind::Invoke,
        UiSemanticAction::Toggle { .. } => UiSemanticActionKind::Toggle,
        UiSemanticAction::Select => UiSemanticActionKind::Select,
        UiSemanticAction::Focus => UiSemanticActionKind::Focus,
        UiSemanticAction::SetValue { value } if value.len() <= 65536 && !value.contains('\0') => {
            UiSemanticActionKind::SetValue
        }
        UiSemanticAction::SetValue { .. } | UiSemanticAction::Scroll { .. } => {
            return Err(error("AT-SPI action is unsupported or exceeds its bound"));
        }
    };
    if !supported(bus, object, &role, &states)
        .await?
        .contains(&required)
    {
        return Err(error("AT-SPI target does not support the approved action"));
    }
    Ok(states)
}

pub(crate) fn preflight_action(
    pid: u32,
    executable: &str,
    fingerprint: &str,
    action: &UiSemanticAction,
) -> Result<()> {
    let (executable, fingerprint, action) = (
        executable.to_owned(),
        fingerprint.to_owned(),
        action.clone(),
    );
    native_ui_identity::run(move || {
        bounded(async move {
            let bus = Bus::connect().await?;
            let object = locate(&bus, pid, &executable, &fingerprint).await?;
            validate(&bus, &object, &action).await?;
            bus.unchanged().await
        })
    })
}

pub(crate) fn apply_action(
    pid: u32,
    executable: &str,
    fingerprint: &str,
    action: &UiSemanticAction,
) -> Result<AppliedAction> {
    let (executable, fingerprint, action) = (
        executable.to_owned(),
        fingerprint.to_owned(),
        action.clone(),
    );
    native_ui_identity::run(move || {
        bounded(async move {
            let bus = Bus::connect().await?;
            let object = locate(&bus, pid, &executable, &fingerprint).await?;
            let before = validate(&bus, &object, &action).await?;
            if matches!(&action, UiSemanticAction::Toggle { desired } if state(&before, 4) == *desired)
            {
                verification::revalidate_target(
                    &bus,
                    &object,
                    pid,
                    &executable,
                    &fingerprint,
                    &action,
                )
                .await?;
                if !verification::read_back(&bus, &object, &action).await? {
                    return Err(error(
                        "AT-SPI checked state changed during no-op verification",
                    ));
                }
                verification::revalidate_target(
                    &bus,
                    &object,
                    pid,
                    &executable,
                    &fingerprint,
                    &action,
                )
                .await?;
                return Ok(AppliedAction {
                    changed: false,
                    verified: true,
                    summary: "Control already has the requested checked state".into(),
                });
            }
            bus.unchanged().await?;
            let call = match &action {
                UiSemanticAction::Invoke | UiSemanticAction::Toggle { .. } => {
                    let proxy = Proxy::new(
                        &bus.connection,
                        object.0.as_str(),
                        object.1.as_str(),
                        "org.a11y.atspi.Action",
                    )
                    .await
                    .map_err(error)?;
                    let actions: Vec<(String, String, String)> =
                        proxy.call("GetActions", &()).await.map_err(error)?;
                    let index = actions
                        .iter()
                        .position(|(name, _, _)| {
                            matches!(name.as_str(), "click" | "press" | "activate")
                        })
                        .ok_or_else(|| error("AT-SPI action changed"))?
                        as i32;
                    verification::revalidate_target(
                        &bus,
                        &object,
                        pid,
                        &executable,
                        &fingerprint,
                        &action,
                    )
                    .await?;
                    native_ui_identity::check_mutation()?;
                    proxy
                        .call::<_, _, bool>("DoAction", &(index,))
                        .await
                        .map_err(error)
                }
                UiSemanticAction::SetValue { value } => {
                    let proxy = Proxy::new(
                        &bus.connection,
                        object.0.as_str(),
                        object.1.as_str(),
                        "org.a11y.atspi.EditableText",
                    )
                    .await
                    .map_err(error)?;
                    verification::revalidate_target(
                        &bus,
                        &object,
                        pid,
                        &executable,
                        &fingerprint,
                        &action,
                    )
                    .await?;
                    native_ui_identity::check_mutation()?;
                    proxy
                        .call::<_, _, bool>("SetTextContents", &(value,))
                        .await
                        .map_err(error)
                }
                UiSemanticAction::Focus => {
                    let proxy = Proxy::new(
                        &bus.connection,
                        object.0.as_str(),
                        object.1.as_str(),
                        "org.a11y.atspi.Component",
                    )
                    .await
                    .map_err(error)?;
                    verification::revalidate_target(
                        &bus,
                        &object,
                        pid,
                        &executable,
                        &fingerprint,
                        &action,
                    )
                    .await?;
                    native_ui_identity::check_mutation()?;
                    proxy
                        .call::<_, _, bool>("GrabFocus", &())
                        .await
                        .map_err(error)
                }
                UiSemanticAction::Select => {
                    let accessible = bus.proxy(&object).await?;
                    let parent: Object = accessible.get_property("Parent").await.map_err(error)?;
                    if parent.0 != object.0 {
                        return Err(error("Selection parent belongs to another application"));
                    }
                    let index: i32 = accessible
                        .call("GetIndexInParent", &())
                        .await
                        .map_err(error)?;
                    if index < 0 {
                        return Err(error("Selection child index is invalid"));
                    }
                    let proxy = Proxy::new(
                        &bus.connection,
                        parent.0.as_str(),
                        parent.1.as_str(),
                        "org.a11y.atspi.Selection",
                    )
                    .await
                    .map_err(error)?;
                    verification::revalidate_target(
                        &bus,
                        &object,
                        pid,
                        &executable,
                        &fingerprint,
                        &action,
                    )
                    .await?;
                    let children: Vec<Object> = bus
                        .proxy(&parent)
                        .await?
                        .call("GetChildren", &())
                        .await
                        .map_err(error)?;
                    if children.len() > 4096 || children.get(index as usize) != Some(&object) {
                        return Err(error("Selection child moved before native submission"));
                    }
                    native_ui_identity::check_mutation()?;
                    proxy
                        .call::<_, _, bool>("SelectChild", &(index,))
                        .await
                        .map_err(error)
                }
                UiSemanticAction::Scroll { .. } => {
                    return Err(error("AT-SPI scroll is unsupported"));
                }
            };
            if !matches!(call, Ok(true)) {
                // The outer guarded executor has entered native execution. Its
                // error path reports OutcomeUnknown, never a replayable failure.
                return Err(error(
                    "AT-SPI native action has no reliable completion receipt; outcome unknown; do not replay",
                ));
            }
            // A failed reply or read-back cannot undo a submitted native action.
            // Identity checks surround read-back so a replacement object cannot
            // validate an earlier object's action, and the summary uses the final verdict.
            let verified = verification::verify_readback(
                verification::revalidate_target(
                    &bus,
                    &object,
                    pid,
                    &executable,
                    &fingerprint,
                    &action,
                ),
                verification::read_back(&bus, &object, &action),
                verification::revalidate_target(
                    &bus,
                    &object,
                    pid,
                    &executable,
                    &fingerprint,
                    &action,
                ),
            )
            .await;
            Ok(verification::submitted_result(verified))
        })
    })
}
