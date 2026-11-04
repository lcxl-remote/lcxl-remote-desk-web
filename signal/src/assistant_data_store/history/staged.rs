//! Prepare every original source read before the caller's native CAS. Final
//! history writes reuse only those rows and this transaction's own writes.

use super::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

mod inspect;
mod write;

pub(crate) struct CaptureRequest<'a> {
    pub capture: &'a HistoryCapture<'a>,
    pub omit_policy: bool,
}

struct Inspection {
    existing_origin: Option<origin::Model>,
    existing_conversation: Option<conversation::Model>,
    new_parts: Vec<(usize, String)>,
    runtime: Option<runtime::Model>,
    existing_binding: Option<binding::Model>,
}

struct Written {
    runtime: runtime::Model,
    conversation: conversation::Model,
    binding: Option<binding::Model>,
}

enum Unit<'a> {
    Ready {
        capture: &'a HistoryCapture<'a>,
        prepared: Box<PreparedHistoryCapture>,
        inspected: Box<Inspection>,
    },
    Gap {
        capture: &'a HistoryCapture<'a>,
        reason: &'static str,
        admission: HistoryAdmission,
    },
}

#[derive(Default)]
struct WriteState {
    runtime: Option<runtime::Model>,
    conversations: BTreeMap<String, Option<conversation::Model>>,
    bindings: BTreeMap<String, Option<binding::Model>>,
}

/// Neither serializable nor a general history-write authorization. The caller
/// holds the original actor/source fences and completes native writes first.
pub(crate) struct PendingHistoryBatch<'db, 'source> {
    db: &'db DatabaseTransaction,
    units: Vec<Unit<'source>>,
    initial: WriteState,
    policy: AssistantDataPolicy,
}

pub(crate) async fn prepare_many_on<'db, 'source>(
    db: &'db DatabaseTransaction,
    captures: &[&'source HistoryCapture<'source>],
    policy: &AssistantDataPolicy,
) -> Result<Box<PendingHistoryBatch<'db, 'source>>, AssistantStoreError> {
    let requests = captures
        .iter()
        .map(|capture| CaptureRequest {
            capture,
            omit_policy: false,
        })
        .collect::<Vec<_>>();
    prepare_requests_on(db, &requests, policy).await
}

pub(crate) async fn prepare_requests_on<'db, 'source>(
    db: &'db DatabaseTransaction,
    requests: &[CaptureRequest<'source>],
    policy: &AssistantDataPolicy,
) -> Result<Box<PendingHistoryBatch<'db, 'source>>, AssistantStoreError> {
    let captures = requests
        .iter()
        .map(|request| request.capture)
        .collect::<Vec<_>>();
    if captures.len() > policy.limits.processing.history_capture_batch_size as usize
        || captures
            .windows(2)
            .any(|pair| pair[0].actor_id != pair[1].actor_id)
    {
        return Err(AssistantStoreError::Invalid(
            "invalid history capture batch",
        ));
    }
    let mut units = Vec::with_capacity(captures.len());
    let mut initial = WriteState::default();
    let mut identities = BTreeSet::new();
    for request in requests {
        let capture = request.capture;
        if request.omit_policy {
            units.push(Unit::Gap {
                capture,
                reason: "omitted_policy",
                admission: HistoryAdmission::OmittedPolicy,
            });
            continue;
        }
        let prepared = match prepare_capture(capture, policy) {
            Ok(Some(value)) => Box::new(value),
            Ok(None) => {
                units.push(Unit::Gap {
                    capture,
                    reason: "omitted_policy",
                    admission: HistoryAdmission::OmittedPolicy,
                });
                continue;
            }
            Err(_) => {
                units.push(Unit::Gap {
                    capture,
                    reason: "omitted_error",
                    admission: HistoryAdmission::OmittedError,
                });
                continue;
            }
        };
        // A savepoint contains a recoverable SQL error without aborting the
        // original native transaction. It does not replace any original SELECT.
        let read = db.begin().await?;
        let inspected = match desk_diagnose_core::future::boxed(|| {
            inspect::inspect_on(&read, capture, &prepared)
        })
        .await
        {
            Ok(value) => {
                read.commit().await?;
                Box::new(value)
            }
            Err(error) => {
                read.rollback().await?;
                let (reason, admission) = omission(error);
                units.push(Unit::Gap {
                    capture,
                    reason,
                    admission,
                });
                continue;
            }
        };
        if !identities.insert(prepared.origin_id.clone()) {
            return Err(AssistantStoreError::Invalid(
                "duplicate native history unit in one capture batch",
            ));
        }
        if let Some(row) = &inspected.runtime {
            if initial.runtime.as_ref().is_some_and(|old| old != row) {
                return Err(AssistantStoreError::Inaccessible);
            }
            initial.runtime = Some(row.clone());
        }
        if initial
            .bindings
            .get(capture.device_ref)
            .is_some_and(|original| original != &inspected.existing_binding)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        initial.bindings.insert(
            capture.device_ref.into(),
            inspected.existing_binding.clone(),
        );
        if let Some(old) = initial.conversations.insert(
            capture.conversation_id.into(),
            inspected.existing_conversation.clone(),
        ) && old != inspected.existing_conversation
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        units.push(Unit::Ready {
            capture,
            prepared,
            inspected,
        });
    }
    Ok(Box::new(PendingHistoryBatch {
        db,
        units,
        initial,
        policy: policy.clone(),
    }))
}

impl PendingHistoryBatch<'_, '_> {
    pub(crate) async fn finalize_on(
        self: Box<Self>,
    ) -> Result<Vec<CaptureOutcome>, AssistantStoreError> {
        let Self {
            db,
            units,
            initial: mut state,
            policy,
        } = *self;
        let mut outcomes = Vec::with_capacity(units.len());
        for unit in units {
            let (capture, prepared, inspected) = match unit {
                Unit::Gap {
                    capture,
                    reason,
                    admission,
                } => {
                    record_gap_on(db, capture, &policy, reason).await?;
                    outcomes.push(CaptureOutcome {
                        admission,
                        source_ids: Vec::new(),
                        partial: true,
                    });
                    continue;
                }
                Unit::Ready {
                    capture,
                    prepared,
                    inspected,
                } => (capture, prepared, inspected),
            };
            if inspected.new_parts.is_empty() {
                outcomes.push(admitted(*prepared));
                continue;
            }
            let runtime = state
                .runtime
                .as_ref()
                .ok_or(AssistantStoreError::Inaccessible)?
                .clone();
            let conversation = state
                .conversations
                .get(capture.conversation_id)
                .ok_or(AssistantStoreError::Inaccessible)?
                .clone();
            let binding = state
                .bindings
                .get(capture.device_ref)
                .ok_or(AssistantStoreError::Inaccessible)?
                .clone();
            let savepoint = db.begin().await?;
            let written = desk_diagnose_core::future::send_phase(|| {
                write::write_on(
                    &savepoint,
                    capture,
                    &prepared,
                    &inspected,
                    runtime,
                    conversation,
                    binding,
                    &policy,
                )
            })
            .await;
            match written {
                Ok(written) => {
                    savepoint.commit().await?;
                    // Advance from actual successful writes only. A rolled-back
                    // unit cannot consume a generation or create a cached row.
                    state.runtime = Some(written.runtime);
                    state
                        .conversations
                        .insert(capture.conversation_id.into(), Some(written.conversation));
                    state
                        .bindings
                        .insert(capture.device_ref.into(), written.binding);

                    outcomes.push(admitted(*prepared));
                }
                Err(error) => {
                    savepoint.rollback().await?;
                    let (reason, admission) = omission(error);
                    record_gap_on(db, capture, &policy, reason).await?;
                    outcomes.push(CaptureOutcome {
                        admission,
                        source_ids: Vec::new(),
                        partial: true,
                    });
                }
            }
        }
        Ok(outcomes)
    }
}

fn admitted(prepared: PreparedHistoryCapture) -> CaptureOutcome {
    CaptureOutcome {
        admission: if prepared.redacted {
            HistoryAdmission::Redacted
        } else {
            HistoryAdmission::Indexed
        },
        source_ids: prepared.source_ids,
        partial: prepared.index.partial,
    }
}

fn omission(error: AdmissionFailure) -> (&'static str, HistoryAdmission) {
    match error {
        AdmissionFailure::Capacity => ("omitted_capacity", HistoryAdmission::OmittedCapacity),
        AdmissionFailure::Policy => ("omitted_policy", HistoryAdmission::OmittedPolicy),
        AdmissionFailure::Storage(error) => {
            drop(error);
            ("omitted_error", HistoryAdmission::OmittedError)
        }
    }
}
