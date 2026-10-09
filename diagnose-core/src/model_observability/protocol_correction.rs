//! Observes existing safe recovery branches without changing model requests.

use super::correction::CorrectionFence;
use super::correction_group::CorrectionCategory;
use super::*;
use sha2::{Digest, Sha256};

closed_enum!(ProtocolCorrectionReason {
    EmptyResponse,
    TruncatedOutput,
    CompletionInterpretation,
    PermissionProtocol,
    PermissionPlanProtocol,
    PermissionActionMissing,
    GoalControlMissing,
    ToolChoiceFallback
});
closed_enum!(ProtocolCorrectionCheck {
    Passed,
    Rejected,
    Unavailable,
    NoResponse
});

impl ProtocolCorrectionReason {
    pub fn category(self) -> CorrectionCategory {
        match self {
            Self::PermissionProtocol
            | Self::PermissionPlanProtocol
            | Self::PermissionActionMissing => CorrectionCategory::Approval,
            _ => CorrectionCategory::Protocol,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProtocolCorrectionFact {
    Feedback {
        source_call_id: String,
        fence: Option<CorrectionFence>,
        reason: ProtocolCorrectionReason,
    },
    Projected {
        source_call_id: String,
        next_call_id: String,
        fence: Option<CorrectionFence>,
    },
    Returned {
        source_call_id: String,
        next_call_id: String,
        fence: Option<CorrectionFence>,
    },
    Response {
        source_call_id: String,
        next_call_id: String,
        fence: Option<CorrectionFence>,
        check: ProtocolCorrectionCheck,
    },
}

impl ProtocolCorrectionFact {
    pub fn source_call_id(&self) -> &str {
        match self {
            Self::Feedback { source_call_id, .. }
            | Self::Projected { source_call_id, .. }
            | Self::Returned { source_call_id, .. }
            | Self::Response { source_call_id, .. } => source_call_id,
        }
    }
    pub fn is_bounded(&self) -> bool {
        let identity = |value: &str| {
            !value.is_empty()
                && value.len() <= 192
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        };
        let (fence, next) = match self {
            Self::Feedback { fence, .. } => (fence, None),
            Self::Projected {
                fence,
                next_call_id,
                ..
            }
            | Self::Returned {
                fence,
                next_call_id,
                ..
            }
            | Self::Response {
                fence,
                next_call_id,
                ..
            } => (fence, Some(next_call_id)),
        };
        identity(self.source_call_id())
            && fence.as_ref().is_none_or(CorrectionFence::is_bounded)
            && next.is_none_or(|next| identity(next) && next != self.source_call_id())
    }
    fn identity(&self) -> String {
        let (stage, next) = match self {
            Self::Feedback { .. } => ("feedback", ""),
            Self::Projected { next_call_id, .. } => ("projected", next_call_id.as_str()),
            Self::Returned { next_call_id, .. } => ("returned", next_call_id.as_str()),
            Self::Response { next_call_id, .. } => ("response", next_call_id.as_str()),
        };
        let mut hash = Sha256::new();
        for value in [self.source_call_id(), stage, next] {
            hash.update((value.len() as u32).to_le_bytes());
            hash.update(value.as_bytes());
        }
        format!("protocol_correction.{:x}", hash.finalize())
    }
}

/// This transient cue exists only beside an already selected business retry.
/// The authoritative opportunity and first projection live in metric storage.
pub struct PendingProtocolCorrection {
    context: ObservationContext,
    pub reason: ProtocolCorrectionReason,
}

impl PendingProtocolCorrection {
    pub fn open(
        context: Option<ObservationContext>,
        fence: Option<CorrectionFence>,
        reason: ProtocolCorrectionReason,
    ) -> Option<Self> {
        let context = context?;
        submit(
            &context,
            ProtocolCorrectionFact::Feedback {
                source_call_id: context.call_id.clone(),
                fence,
                reason,
            },
        );
        Some(Self { context, reason })
    }

    pub fn project(
        self,
        next: Option<ObservationContext>,
        fence: Option<CorrectionFence>,
        cue_present: bool,
    ) -> Option<ProtocolCorrectionGuard> {
        if !cue_present {
            return None;
        }
        let next = next?;
        let fact = ProtocolCorrectionFact::Projected {
            source_call_id: self.context.call_id.clone(),
            next_call_id: next.call_id.clone(),
            fence: fence.clone(),
        };
        if !fact.is_bounded() {
            return None;
        }
        submit(&self.context, fact);
        Some(ProtocolCorrectionGuard {
            context: self.context,
            next_call_id: next.call_id,
            fence,
            reason: self.reason,
            dial_failed: false,
            finished: false,
        })
    }
}

pub struct ProtocolCorrectionGuard {
    context: ObservationContext,
    next_call_id: String,
    fence: Option<CorrectionFence>,
    pub reason: ProtocolCorrectionReason,
    dial_failed: bool,
    finished: bool,
}

impl ProtocolCorrectionGuard {
    pub fn dial_result(&mut self, returned: bool) {
        self.dial_failed = !returned;
        if returned {
            submit(
                &self.context,
                ProtocolCorrectionFact::Returned {
                    source_call_id: self.context.call_id.clone(),
                    next_call_id: self.next_call_id.clone(),
                    fence: self.fence.clone(),
                },
            );
        }
    }
    pub fn check(&mut self, check: ProtocolCorrectionCheck) {
        if self.finished {
            return;
        }
        self.finished = true;
        submit(
            &self.context,
            ProtocolCorrectionFact::Response {
                source_call_id: self.context.call_id.clone(),
                next_call_id: self.next_call_id.clone(),
                fence: self.fence.clone(),
                check,
            },
        );
    }
}

impl Drop for ProtocolCorrectionGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.check(if self.dial_failed {
                ProtocolCorrectionCheck::NoResponse
            } else {
                ProtocolCorrectionCheck::Unavailable
            });
        }
    }
}

fn submit(context: &ObservationContext, fact: ProtocolCorrectionFact) {
    if !fact.is_bounded() {
        return;
    }
    let Some(source) = ObservationAlias::model_call(fact.source_call_id()) else {
        return;
    };
    context.emit_related(
        fact.identity(),
        ObservationPhase::Stage,
        0,
        tool::now_ms(),
        ObservationPayload::Call(CallSnapshot::default()),
        Some(ObservationRelation::ProtocolCorrection { source, fact }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[derive(Default)]
    struct Recorder(Mutex<Vec<ObservationEvent>>);
    impl ObservabilitySeam for Recorder {
        fn submit(&self, event: ObservationEvent) {
            self.0.lock().unwrap().push(event);
        }
    }
    fn context(id: &str, recorder: Arc<dyn ObservabilitySeam>) -> ObservationContext {
        ObservationContext::new(
            id.into(),
            1,
            Attribution {
                provider_id: "provider".into(),
                model_id: "model".into(),
                model_name: "Model".into(),
                configuration_revision: "1".into(),
                contract_revision: "1".into(),
                surface: Surface::Assistant,
                purpose: Purpose::Agent,
                origin: Origin::User,
                configuration_scope: ConfigurationScope::Local,
                protocol: Protocol::OpenAiChatCompletions,
            },
            recorder,
        )
    }
    #[test]
    fn only_projected_cues_create_response_facts_and_drop_is_not_a_guessed_model_failure() {
        let recorder = Arc::new(Recorder::default());
        let cue = PendingProtocolCorrection::open(
            Some(context("source", recorder.clone())),
            None,
            ProtocolCorrectionReason::EmptyResponse,
        )
        .unwrap();
        assert!(
            cue.project(
                Some(context("not-projected", recorder.clone())),
                None,
                false
            )
            .is_none()
        );
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
        let cue = PendingProtocolCorrection::open(
            Some(context("next-source", recorder.clone())),
            None,
            ProtocolCorrectionReason::EmptyResponse,
        )
        .unwrap();
        drop(cue.project(Some(context("projected", recorder.clone())), None, true));
        let events = recorder.0.lock().unwrap();
        assert!(events.iter().all(ObservationEvent::is_bounded));
        assert!(matches!(
            &events.last().unwrap().relation,
            Some(ObservationRelation::ProtocolCorrection {
                fact: ProtocolCorrectionFact::Response {
                    check: ProtocolCorrectionCheck::Unavailable,
                    ..
                },
                ..
            })
        ));
    }
    #[test]
    fn explicit_dial_failure_and_checked_rejection_have_distinct_terminal_facts() {
        for (returned, check) in [
            (false, ProtocolCorrectionCheck::NoResponse),
            (true, ProtocolCorrectionCheck::Rejected),
        ] {
            let recorder = Arc::new(Recorder::default());
            let cue = PendingProtocolCorrection::open(
                Some(context("source", recorder.clone())),
                None,
                ProtocolCorrectionReason::PermissionPlanProtocol,
            )
            .unwrap();
            let mut guard = cue
                .project(Some(context("next", recorder.clone())), None, true)
                .unwrap();
            guard.dial_result(returned);
            if returned {
                guard.check(check);
                guard.check(ProtocolCorrectionCheck::Passed);
            }
            drop(guard);
            let events = recorder.0.lock().unwrap();
            assert_eq!(events.len(), if returned { 4 } else { 3 });
            assert!(
                matches!(&events.last().unwrap().relation,Some(ObservationRelation::ProtocolCorrection { fact:ProtocolCorrectionFact::Response { check:recorded,.. },.. }) if *recorded==check)
            );
            assert_eq!(
                ProtocolCorrectionReason::PermissionPlanProtocol.category(),
                CorrectionCategory::Approval
            );
        }
    }
    #[test]
    fn observation_panic_does_not_change_a_selected_retry_or_checked_result() {
        struct Broken;
        impl ObservabilitySeam for Broken {
            fn submit(&self, _: ObservationEvent) {
                panic!("unavailable");
            }
        }
        let cue = PendingProtocolCorrection::open(
            Some(context("source", Arc::new(Broken))),
            None,
            ProtocolCorrectionReason::TruncatedOutput,
        )
        .unwrap();
        let mut guard = cue
            .project(Some(context("next", Arc::new(Broken))), None, true)
            .unwrap();
        guard.dial_result(true);
        guard.check(ProtocolCorrectionCheck::Passed);
    }
}
