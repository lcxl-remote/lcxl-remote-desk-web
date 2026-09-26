//! Read-back verification stays bound to the original application and object.
use super::*;

pub(super) async fn revalidate_target(
    bus: &Bus,
    object: &Object,
    pid: u32,
    executable: &str,
    fingerprint: &str,
    action: &UiSemanticAction,
) -> Result<()> {
    bus.unchanged().await?;
    if locate(bus, pid, executable, fingerprint).await? != *object {
        return Err(error("AT-SPI target changed during action"));
    }
    validate(bus, object, action).await?;
    Ok(())
}

pub(super) async fn read_back(
    bus: &Bus,
    object: &Object,
    action: &UiSemanticAction,
) -> Result<bool> {
    // Re-read role and state before accessing text. A formerly editable control
    // may have become protected while the native call was in flight.
    let states = validate(bus, object, action).await?;
    Ok(match action {
        UiSemanticAction::Toggle { desired } => state(&states, 4) == *desired,
        UiSemanticAction::Focus => state(&states, 12),
        UiSemanticAction::Select => state(&states, 23),
        UiSemanticAction::SetValue { value } => {
            let proxy = Proxy::new(
                &bus.connection,
                object.0.as_str(),
                object.1.as_str(),
                "org.a11y.atspi.Text",
            )
            .await
            .map_err(error)?;
            let text: String = proxy
                .call("GetText", &(0i32, value.chars().count() as i32 + 1))
                .await
                .map_err(error)?;
            text == *value
        }
        UiSemanticAction::Invoke | UiSemanticAction::Scroll { .. } => false,
    })
}

pub(super) fn submitted_result(verified: bool) -> AppliedAction {
    AppliedAction {
        changed: true,
        verified,
        summary: if verified {
            "AT-SPI action state verified"
        } else {
            "AT-SPI action submitted; outcome unverified; do not replay"
        }
        .into(),
    }
}

pub(super) async fn verify_readback(
    before: impl Future<Output = Result<()>>,
    observation: impl Future<Output = Result<bool>>,
    after: impl Future<Output = Result<()>>,
) -> bool {
    if before.await.is_err() {
        return false;
    }
    let Ok(true) = observation.await else {
        return false;
    };
    after.await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn matching_state_cannot_verify_an_object_that_changed_after_readback() {
        let verified = verify_readback(async { Ok(()) }, async { Ok(true) }, async {
            Err(error("application owner changed after read-back"))
        })
        .await;
        let output = submitted_result(verified);
        assert!(output.changed);
        assert!(!output.verified);
        assert!(output.summary.contains("do not replay"));
        assert!(!output.summary.contains("state verified"));
    }

    #[tokio::test]
    async fn changed_or_protected_target_is_not_read_and_dbus_errors_remain_unverified() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let read = AtomicBool::new(false);
        let verified = verify_readback(
            async { Err(error("target became protected")) },
            async {
                read.store(true, Ordering::SeqCst);
                Ok(true)
            },
            async { Ok(()) },
        )
        .await;
        assert!(!verified);
        assert!(!read.load(Ordering::SeqCst));
        assert!(
            !verify_readback(
                async { Ok(()) },
                async { Err(error("D-Bus reply lost")) },
                async { Ok(()) }
            )
            .await
        );
        assert!(verify_readback(async { Ok(()) }, async { Ok(true) }, async { Ok(()) }).await);
    }
}
