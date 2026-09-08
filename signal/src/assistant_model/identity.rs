//! Model evidence identities derived from durable, owner-bound session facts.
use sha2::{Digest, Sha256};

pub(crate) enum ModelExportSource<'a> {
    Input(&'a str),
    Turn(&'a str),
}

/// Inputs must come from the accepted session/claim. A transport request ID is
/// intentionally unnecessary, so later history readers can reproduce the scope.
pub(crate) fn model_export_id(
    actor: &str,
    device: &str,
    conversation: &str,
    source: ModelExportSource<'_>,
) -> String {
    let (kind, id) = match source {
        ModelExportSource::Input(id) => ("input", id),
        ModelExportSource::Turn(id) => ("turn", id),
    };
    let mut hash = Sha256::new();
    for field in [
        "assistant-model-export-v1",
        actor,
        device,
        conversation,
        kind,
        id,
    ] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    format!("assistant-export-{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn evidence_identity_is_reproducible_and_bound_to_durable_source() {
        let original = model_export_id(
            "1",
            "device",
            "conversation",
            ModelExportSource::Input("message"),
        );
        assert_eq!(
            original,
            model_export_id(
                "1",
                "device",
                "conversation",
                ModelExportSource::Input("message")
            )
        );
        for changed in [
            model_export_id(
                "2",
                "device",
                "conversation",
                ModelExportSource::Input("message"),
            ),
            model_export_id(
                "1",
                "other",
                "conversation",
                ModelExportSource::Input("message"),
            ),
            model_export_id("1", "device", "other", ModelExportSource::Input("message")),
            model_export_id(
                "1",
                "device",
                "conversation",
                ModelExportSource::Input("other"),
            ),
            model_export_id(
                "1",
                "device",
                "conversation",
                ModelExportSource::Turn("message"),
            ),
        ] {
            assert_ne!(original, changed);
        }
        assert_ne!(
            model_export_id("a:b", "c", "run", ModelExportSource::Input("input")),
            model_export_id("a", "b:c", "run", ModelExportSource::Input("input")),
        );
    }
}
