//! Construct continuation futures off the composition poll stack.
use super::*;
use desk_diagnose_core::session::PersistedAgentSession;
use std::{future::Future, pin::Pin};

// The caller retains the same session, dependencies and sink lifetimes. Boxing
// changes storage only: no detached task, new claim, replay or cancellation gap.
#[inline(never)]
pub(super) fn drive<'a>(
    deps: &'a LoopDeps<'a>,
    session: PersistedAgentSession,
    run_id: &'a str,
    permission_request_id: Option<&'a str>,
    fresh: Option<&'a fresh::FreshContext>,
    sink: &'a mut dyn TurnSink,
) -> Pin<Box<dyn Future<Output = Result<LoopOutcome, AgentError>> + 'a>> {
    use desk_diagnose_core::agent_loop::*;
    if let Some(fresh) = fresh {
        let Some(creation) = fresh.creation.as_ref() else {
            return Box::pin(async { Err(transport_error("published source was not bound")) });
        };
        if let Some(wait_id) = fresh.children_wait_id.as_deref() {
            Box::pin(resume_claimed_fresh_task_children_turn(
                deps,
                session,
                &fresh.contract,
                creation,
                run_id,
                wait_id,
                sink,
            ))
        } else if let Some(message_id) = fresh.children_notification_id.as_deref() {
            Box::pin(resume_claimed_fresh_task_notification_turn(
                deps,
                session,
                &fresh.contract,
                creation,
                run_id,
                message_id,
                sink,
            ))
        } else if let Some(request_id) = fresh.approval_reference.as_deref() {
            Box::pin(resume_claimed_fresh_task_permission_turn(
                deps,
                session,
                &fresh.contract,
                creation,
                run_id,
                request_id,
                sink,
            ))
        } else {
            Box::pin(resume_claimed_fresh_task_turn(
                deps,
                session,
                &fresh.contract,
                creation,
                run_id,
                sink,
            ))
        }
    } else if let Some(request_id) = permission_request_id {
        Box::pin(resume_claimed_scheduled_permission_turn(
            deps, session, run_id, request_id, sink,
        ))
    } else {
        Box::pin(resume_claimed_scheduled_turn(deps, session, run_id, sink))
    }
}
