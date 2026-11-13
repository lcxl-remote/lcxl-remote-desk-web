#!/usr/bin/env python3
"""Enforce current stack hard limits and required execution coverage."""
import argparse
import subprocess
import sys
import report
from pathlib import Path

HERE = Path(__file__).resolve().parent
REQUIRED = {
    'desk-agent-protocol': [
        '^desk_agent_protocol::ai_assistant::memory.*MemoryDraft.*validate_with_limits$',
        '^desk_agent_protocol::ai_assistant::memory.*MemoryWriteRequest.*validate_with_limits$',
        '^desk_agent_protocol::ai_assistant::history.*ConversationSearchRequest.*validate_with_limits$',
    ],
    'desk-signal-facade': [
        '^desk_signal_facade::controller::ai_assistant_session::project_snapshot_message_page$',
        '^desk_signal_facade::controller::ai_assistant_session::project_snapshot_message_window$',
        'desk_signal_facade::controller::ai_assistant_session::SnapshotMessagePageProjection.*with_citations$',
    ],
    'desk-assistant-data-store': [
        '^desk_assistant_data_store::memory_write::commit_id$',
        '^desk_assistant_data_store::memory_write::result$',
        '^desk_assistant_data_store::projection::decode_row$',
        '^desk_assistant_data_store::retrieval_cache::decode_row$',
        '^desk_assistant_data_store::memory_control::project_receipt$',
    ],
    'desk-diagnose-core': [
        '^desk_diagnose_core::model_input::check_at$',
        '^desk_diagnose_core::ai_assistant::data_tools::(?:decode_error|parse_impl|forget_batch)$',
        '^desk_diagnose_core::ui_model_ids::project_request$',
        '^desk_diagnose_core::ui_model_ids::resolve_session_call_with_context$',
        '^desk_diagnose_core::ui_model_ids::resolve_recorded_call$',
        '^desk_diagnose_core::schedule::proposal::draft$',
        '^desk_diagnose_core::schedule::proposal::spec$',
        '^desk_diagnose_core::schedule::proposal::invalid_input$',
        '^desk_diagnose_core::schedule::management_tools::parse$',
        '^desk_diagnose_core::provider_preflight::read::output_digest$',
        '^desk_diagnose_core::provider_preflight::read::error_output_digest$',
        '^desk_diagnose_core::schedule::rehearsal::read_sources::project_tool_result_source$',
        '^desk_diagnose_core::ui_references::UiReferenceState::resolve_conversation_request$',
        '^desk_diagnose_core::ui_model_ids::project_tool_message$',
        '^desk_diagnose_core::conversation_title::request$',
        '^desk_diagnose_core::conversation_title::parse$',
        '^desk_diagnose_core::conversation_title::fallback$',
        r'^desk_diagnose_core::agent_loop::title::generate::_\{\{closure\}\}::_\{\{closure\}\}$',

        '^desk_diagnose_core::ai_assistant::data_control::memory_forget::commit_memory_forget_group$',
        '^desk_diagnose_core::ai_assistant::data_control::memory_forget::read_memory_forget_group$',
        r'^desk_diagnose_core::agent_loop::assistant_data::dispatch_forget::_\{\{closure\}\}$',
        'agent_loop::phases::prepare_iteration', 'PersistedAgentSession', 'future::boxed',
                           'ContextProtectionSet.*validate_references',
                           '^desk_diagnose_core::schedule::published_input::TaskPromptRetention::validate$',
                           '^desk_diagnose_core::schedule::published_input::is_published_input_label$',
                           '^desk_diagnose_core::ai_assistant::native_history::NativeHistoryMetadata::validate$',
                           '^desk_diagnose_core::ai_assistant::source::prepare_capture$',
                           '^desk_diagnose_core::ai_assistant::source_registry::snapshot::registered_inputs$',
                           '^desk_diagnose_core::ai_assistant::source_registry::snapshot::native_control$',],
    'desk-signal': [r'^desk_signal::assistant_data_store::memory_control::native::forget_on::_\{\{closure\}\}$',
                    'ai_assistant_orchestrator::prepare_composed_turn', 'remote_tool_edge::.*semantic',
                    '^desk_signal::agent_subagent_store::observation::save_main_delegation_session.*closure',
                    '^desk_signal::agent_subagent_store::observation::save_main_history_on.*closure',
                    '^desk_diagnose_core::agent_loop::fresh_task::resume_claimed_fresh_task_turn.*closure',
                    '^desk_diagnose_core::schedule::dispatch::dispatch_continuation.*closure',
                    '^desk_diagnose_core::schedule::dispatch::dispatch_fresh.*closure',
                    'desk_signal::schedule_executor::continuation::ContinuationHost.*ContinuationDispatchHost.*run_turn.*closure',
                    'desk_signal::schedule_executor::fresh::FreshHost.*FreshDispatchHost.*run_turn.*closure',
                    'review_budget::reserve_review_attempt_on', 'approval_reviewer::call_claimed',
                    'assistant_model', 'SignalModelSeam', 'capability_grant_store::task_authority::match_current_on',
                    '^desk_signal::assistant_data_store::output_archive::gc::collect_actor_transaction.*closure',
                    '^desk_signal::assistant_data_store::output_archive::gc::collect_archive_on.*closure',
                    '^desk_signal::assistant_data_store::owner_source::history_source_on.*closure',
                    '^desk_signal::assistant_data_store::model_sources::validate_history_on.*closure',
                    '^desk_signal::assistant_data_store::history::staged::write::write_on.*closure',
                    '^desk_signal::assistant_data_store::history::staged::write::write_conversation_on.*closure',
                    '^desk_signal::assistant_data_store::history::staged::write::write_binding_on.*closure',
                    '^desk_signal::schedule_store::ScheduleStore::create_draft_on.*closure',
                    '^desk_signal::schedule_store::ScheduleStore::capture_retrieval_consent_on.*closure',
                    '^desk_signal::schedule_store::ScheduleStore::capture_definition_consent_on.*closure',
                    '^desk_signal::schedule_store::fresh_session::_<impl desk_signal::schedule_store::ScheduleStore>::insert_fresh_session_inner.*closure',
                    '^desk_signal::schedule_store::fresh_session::_<impl desk_signal::schedule_store::ScheduleStore>::initialize_fresh_assistant_on.*closure',
                    '^desk_signal::schedule_store::fresh_session::_<impl desk_signal::schedule_store::ScheduleStore>::owner_prompt_retention_on.*closure',
                    '^desk_signal::schedule_store::fresh_session::_<impl desk_signal::schedule_store::ScheduleStore>::record_prompt_retention_on.*closure',
                    '^desk_signal::schedule_store::fresh_session::_<impl desk_signal::schedule_store::ScheduleStore>::published_input_retention_on.*closure',
                    '^desk_signal::assistant_data_store::visible_history::native::inputs::published_on::.*closure.*closure',
                    '^desk_signal::schedule_store::fresh_recovery::_<impl desk_signal::schedule_store::ScheduleStore>::recover_action_free_fresh_task::.*closure.*closure',
                    '^desk_signal::schedule_store::fresh_recovery::_<impl desk_signal::schedule_store::ScheduleStore>::reconcile_expired_fresh_task.*closure',
                    '^desk_signal::schedule_store::fresh_recovery::_<impl desk_signal::schedule_store::ScheduleStore>::settle_expired_fresh_task.*closure',
                    '^desk_signal::schedule_store::fresh_children_recovery::recover.*closure',
                    '^desk_signal::schedule_store::fresh_permission_recovery::recover.*closure',
                    '^desk_signal::agent_subagent_store::scheduled_native::scheduled_native_on.*closure',
                    '^desk_signal::agent_subagent_store::scheduled_native::scheduled_session_on.*closure',
                    '^desk_signal::agent_subagent_store::scheduled_native::scheduled_commands_on.*closure',
                    '^desk_signal::agent_subagent_store::scheduled_native::scheduled_actions_on.*closure',
                    '^desk_signal::capability_grant_store::fresh_recovery::restore_completed_calls.*closure',
                    '^desk_signal::capability_grant_store::fresh_recovery::restore_computer_results_on.*closure',
                    '^desk_signal::capability_grant_store::scheduled_recovery::command::reconcile_inner_on.*closure',
                    '^desk_signal::capability_grant_store::scheduled_recovery::command::validate_command_origin_on.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::settle_fresh_children_wait_with_observer.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::load_fresh_children_wait.*closure',
                    '^desk_signal::schedule_store::cancel::_<impl desk_signal::schedule_store::ScheduleStore>::cancel_run.*closure',
                    '^desk_signal::schedule_store::cancel::_<impl desk_signal::schedule_store::ScheduleStore>::record_run_cancellation.*closure',
                    '^desk_signal::schedule_store::materializer::_<impl desk_signal::schedule_store::ScheduleStore>::scan_wait_expiry_once::.*closure.*closure',
                    '^desk_signal::schedule_store::materializer::_<impl desk_signal::schedule_store::ScheduleStore>::scan_children_wait_settlement_once::.*closure.*closure',
                    '^desk_signal::agent_subagent_store::main_stop::_<impl desk_signal::agent_subagent_store::SubAgentStore>::stop_for_owner.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::prepare_fresh_children_close.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::close_fresh_children_source.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::persist_fresh_children_close.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::reconcile_fresh_children_receipts_inner.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::load_fresh_children_receipts.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::load_fresh_receipt_root.*closure',
                    '^desk_signal::schedule_store::fresh_children_settlement::_<impl desk_signal::schedule_store::ScheduleStore>::persist_fresh_children_receipts.*closure',
                    '^desk_signal::schedule_store::resume_claim::_<impl desk_signal::schedule_store::ScheduleStore>::claim_continuation_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_claim::claim_continuation_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_claim::prepare_continuation_task_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_claim::bind_continuation_session_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_admission::assistant_retrieval_deadline_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_admission::lock_action_session_inner.*closure',
                    '^desk_assistant_data_store::schedule::resume_admission::lock_fresh_action_session_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_admission::validate_continuation_action_on.*closure',
                    '^desk_signal::agent_goal_store::claim_slice::.*closure.*closure',
                    '^desk_signal::agent_goal_store::claim_slice_source_on.*closure',
                    '^desk_signal::agent_goal_store::scheduler_transition.*closure',
                    '^desk_signal::assistant_data_store::accept_input_on.*closure',
                    '^desk_signal::agent_run_event_store::SignalAgentRunEventStore::admit_user_input.*closure',
                    '^desk_signal::agent_run_event_store::prepare_input_goals_on.*closure',
                    '^desk_signal::agent_run_event_store::write_input_goal_events_on.*closure',
                    'agent_run_event_store::.*handled_outcome', 'agent_goal_store::settle_slice_afterimage',
                    'agent_goal_store::persist_slice_on', 'agent_goal_store::validate_slice_session_on'],
    'lcxl-remote-desk-manager': [r'^lcxl_remote_desk_manager::service::assistant_data_store::memory_control::native::forget_on::_\{\{closure\}\}$',
                               'agent_loop_driver::prepare_composed_turn', 'manager_provider_tools::.*prepare_execution',
                               '^lcxl_remote_desk_manager::service::agent_subagent_store::observation::save_main_delegation_session.*closure',
                               '^lcxl_remote_desk_manager::service::agent_subagent_store::observation::save_main_history_on.*closure',
                    '^desk_diagnose_core::agent_loop::fresh_task::resume_claimed_fresh_task_turn.*closure',
                    '^desk_diagnose_core::schedule::dispatch::dispatch_continuation.*closure',
                    '^desk_diagnose_core::schedule::dispatch::dispatch_fresh.*closure',
                    'lcxl_remote_desk_manager::service::schedule_executor::continuation::ContinuationHost.*ContinuationDispatchHost.*run_turn.*closure',
                    'lcxl_remote_desk_manager::service::schedule_executor::fresh::FreshHost.*FreshDispatchHost.*run_turn.*closure',
                    'review_budget::reserve_review_attempt_on', 'agent_approval_reviewer::call_claimed',
                    'agent_model::.*ModelSeam.*call', 'capability_provider_work::dispatch',
                    'capability_grant_store::task_authority::match_current_on',
                    '^lcxl_remote_desk_manager::service::assistant_data_store::output_archive::gc::collect_actor_transaction.*closure',
                    '^lcxl_remote_desk_manager::service::assistant_data_store::output_archive::gc::collect_archive_on.*closure',
                    '^lcxl_remote_desk_manager::service::assistant_data_store::owner_source::history_source_on.*closure',
                    '^lcxl_remote_desk_manager::service::assistant_data_store::model_sources::validate_history_on.*closure',
                    '^lcxl_remote_desk_manager::service::assistant_data_store::history::staged::write::write_on.*closure',
                    '^lcxl_remote_desk_manager::service::assistant_data_store::history::staged::write::write_conversation_on.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::ScheduleStore::create_draft_on.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::ScheduleStore::capture_retrieval_consent_on.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::ScheduleStore::capture_definition_consent_on.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_session::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::insert_fresh_session_inner.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_session::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::initialize_fresh_assistant_on.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_session::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::owner_prompt_retention_on.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_session::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::record_prompt_retention_on.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_session::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::published_input_retention_on.*closure',
                    '^lcxl_remote_desk_manager::service::assistant_data_store::visible_history::native::inputs::published_on::.*closure.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_recovery::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::recover_action_free_fresh_task::.*closure.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_recovery::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::reconcile_expired_fresh_task.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_recovery::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::settle_expired_fresh_task.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_recovery::recover.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_permission_recovery::recover.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::settle_fresh_children_wait_with_observer.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::load_fresh_children_wait.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::cancel::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::cancel_run.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::cancel::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::record_run_cancellation.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::materializer::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::scan_wait_expiry_once::.*closure.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::materializer::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::scan_children_wait_settlement_once::.*closure.*closure',
                    '^lcxl_remote_desk_manager::service::agent_subagent_store::main_stop::_<impl lcxl_remote_desk_manager::service::agent_subagent_store::SubAgentStore>::stop_for_owner.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::prepare_fresh_children_close.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::close_fresh_children_source.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::persist_fresh_children_close.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::reconcile_fresh_children_receipts_inner.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::load_fresh_children_receipts.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::load_fresh_receipt_root.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::fresh_children_settlement::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::persist_fresh_children_receipts.*closure',
                    '^lcxl_remote_desk_manager::service::schedule_store::resume_claim::_<impl lcxl_remote_desk_manager::service::schedule_store::ScheduleStore>::claim_continuation_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_claim::claim_continuation_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_claim::prepare_continuation_task_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_claim::bind_continuation_session_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_admission::assistant_retrieval_deadline_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_admission::lock_action_session.*closure',
                    '^desk_assistant_data_store::schedule::resume_admission::lock_fresh_action_session_on.*closure',
                    '^desk_assistant_data_store::schedule::resume_admission::validate_continuation_action_on.*closure',
                    '^lcxl_remote_desk_manager::service::agent_goal_store::claim_slice::.*closure.*closure',
                    '^lcxl_remote_desk_manager::service::agent_goal_store::claim_slice_source_on.*closure',
                    '^lcxl_remote_desk_manager::service::agent_goal_store::scheduler_transition.*closure',
                    '^lcxl_remote_desk_manager::service::assistant_data_store::accept_input_on.*closure',
                    '^lcxl_remote_desk_manager::service::agent_run_event_store::ManagerRunEventStore::admit_user_input.*closure',
                    '^lcxl_remote_desk_manager::service::agent_run_event_store::persist_input_on.*closure',
                    '^lcxl_remote_desk_manager::service::agent_run_event_store::prepare_input_goals_on.*closure',
                    '^lcxl_remote_desk_manager::service::agent_run_event_store::file_scope::_<impl lcxl_remote_desk_manager::service::agent_run_event_store::ManagerRunEventStore>::update_file_scope_guarded.*closure',
                    '^lcxl_remote_desk_manager::service::agent_run_event_store::file_scope::load_session_on.*closure',
                    '^lcxl_remote_desk_manager::service::agent_run_event_store::file_scope::persist_update_on.*closure',
                    '^lcxl_remote_desk_manager::service::agent_run_event_store::write_input_goal_events_on.*closure',
                    'agent_run_event_store::.*read_user_input_state', 'agent_goal_store::settle_slice_afterimage',
                    'agent_goal_store::persist_slice_on', 'agent_goal_store::validate_slice_session_on'],
}

# Snapshots replace source-driven recovery. Keep execution coverage at adoption,
# current-call authorization and immutable copy-policy resolution.
REQUIRED['desk-diagnose-core'] += [
    '^desk_diagnose_core::ai_assistant::read_projection::ReadProjectionProof::snapshot_envelope$',
    '^desk_diagnose_core::ai_assistant::seam::AssistantModelDataManifest::validate_for$',
]

for _package, _host in [('desk-signal', 'desk_signal'),
                        ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
    for _path in ['visible_history::snapshot::prepare_on',
                  'visible_history::native::resolve_policy_on',
                  'visible_history::native::citation_roots_on',
                  'visible_history::citations::load_on',
                  'visible_history::native::inputs::owner_text_on',
                  'prior_outputs::validate_on', 'model_gate::authorize_on',
                  'model_gate::output::validate_on']:
        REQUIRED[_package].append('^' + _host + '::assistant_data_store::' + _path + '::.*closure.*closure')

# Schedule storage retains its actual native poll, beyond the trait adapter.
for _package, _host in [('desk-signal', 'desk_signal'),
                        ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
    REQUIRED[_package].append('^_<' + _host + r'::agent_session_store::.* as desk_diagnose_core::seam::SessionSeam>::manage_schedule_tool::_\{\{closure\}\}$')

# Direct writes retain physical ownership and require actual execution polls.
for _package, _host in [('desk-signal', 'desk_signal'),
                        ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
    for _operation in ('write', 'write_on'):
        REQUIRED[_package].append('^' + _host + r'::assistant_data_store::memory_write::' + _operation + r'::_\{\{closure\}\}$')
    for _path in ('memory_write::apply_on', 'memory_write::sources::load_on', 'memory_write::receipts::collect_on'):
        REQUIRED[_package].append('^desk_assistant_data_store::' + _path + r'::_\{\{closure\}\}$')
REQUIRED['desk-signal'].append(r'^desk_signal::model_egress_store::.*read_context_output_with_archive_on::_\{\{closure\}\}$')
REQUIRED['lcxl-remote-desk-manager'].append(r'^lcxl_remote_desk_manager::service::ai_call_request::read_context_output_with_archive_on::_\{\{closure\}\}$')

# Naming metadata phases retain the original caller and must expose their real polls.
for _package, _host in [('desk-signal', 'desk_signal'),
                        ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
    for _method in ('load_title_limit', 'store_title'):
        REQUIRED[_package].append('^' + _host + r'::agent_session_store::title::.*' + _method + r'::_\{\{closure\}\}::_\{\{closure\}\}$')

# Unsupported snapshots must retain measured retirement and child-control execution.
for _package, _host in [('desk-signal', 'desk_signal'),
                        ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
    for _path in ('agent_subagent_store::retention::close_root_for_subject_on',
                  'agent_subagent_store::retention::close_child_on',
                  'agent_session_store::management::retire_unsupported_for_expiry'):
        suffix = r'::_\{\{closure\}\}::_\{\{closure\}\}$' if 'retire_unsupported' in _path or 'close_child_on' in _path else r'::_\{\{closure\}\}$'
        REQUIRED[_package].append('^' + _host + '::' + _path + suffix)

# Candidate metadata scans keep the caller's ownership and expose their actual poll.
for _package, _host in [('desk-signal', 'desk_signal'),
                        ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
    REQUIRED[_package].append('^' + _host + r'::agent_subagent_store::candidates::.*root_wait_candidates::_\{\{closure\}\}::_\{\{closure\}\}$')

REQUIRED['desk-diagnose-core'].append('^desk_diagnose_core::permission_grant::apply_automatic_grant_failure$')
REQUIRED['desk-signal'].append(r'^desk_signal::agent_session_store.*decide_permission_request_with_authority::_\{\{closure\}\}$')
REQUIRED['lcxl-remote-desk-manager'].append(r'^lcxl_remote_desk_manager::service::agent_permission_store.*decide_with_authority::_\{\{closure\}\}$')

# The hosts instantiate shared discovery, cursor and single-pass execution polls.
for _package in ('desk-signal', 'lcxl-remote-desk-manager'):
    for _operation in ('due_actors_on', 'claim_on', 'finish_quota_on'):
        REQUIRED[_package].append(
            '^desk_assistant_data_store::output_gc::' + _operation + r'::_\{\{closure\}\}::_\{\{closure\}\}$')



# Historical reads preserve exact version/source polls in both host contexts.
REQUIRED['desk-diagnose-core'].append(
    '^desk_diagnose_core::ai_assistant::retrieval_response::fit_memory_history$')
for _package, _host in [('desk-signal', 'desk_signal'),
                        ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
    for _path in ('memory_history::read_on',
                  'memory_view::read_history_on', 'memory_view::read_proof_on'):
        REQUIRED[_package].append('^' + _host + '::assistant_data_store::' + _path + r'::_\{\{closure\}\}$')
    for _path in ('head_on', 'page_on', 'scan_on'):
        REQUIRED[_package].append('^desk_assistant_data_store::memory_history::' + _path + r'::_\{\{closure\}\}$')


REQUIRED['lcxl-remote-desk-manager'].append(
    r'^lcxl_remote_desk_manager::service::capability_grant_store::read_use::.*::read_use::_\{\{closure\}\}$')

# The trait future owns manifest validation after optimized inlining.
for _package, _store in [
        ('desk-signal', 'desk_signal::agent_session_store::SignalAgentSessionStore'),
        ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service::agent_session_store::AgentSessionStore')]:
    REQUIRED[_package].append(
        '^_<' + _store + r' as desk_diagnose_core::seam::SessionSeam>::assistant_data_request_manifest::_\{\{closure\}\}$')


for _package in ('desk-signal', 'lcxl-remote-desk-manager'):
    REQUIRED[_package].append(r'^desk_signal_facade::service::lease_heartbeat::renew_until_lost::_\{\{closure\}\}::_\{\{closure\}\}$')


def required_patterns(package, profile):
    patterns = list(REQUIRED[package])
    if package == 'desk-diagnose-core' and profile == 'release':
        # LLVM coalesces these identical InvalidInput constructors to one address.
        patterns = [r'^(?:desk_diagnose_core::schedule::proposal::invalid_input|desk_diagnose_core::agent_loop::retrieval_accounting::invalid)$'
                    if pattern == '^desk_diagnose_core::schedule::proposal::invalid_input$' else pattern
                    for pattern in patterns]
    if package == 'desk-diagnose-core' and profile == 'debug':
        patterns.append(r'^desk_diagnose_core::agent_loop::task_permission::pause::_\{\{closure\}\}$')
    if package == 'desk-agent-protocol' and profile == 'debug':
        patterns.append('^desk_agent_protocol::ai_assistant::memory::validate_draft_limits$')
    if profile != 'release' or package not in ('desk-signal', 'lcxl-remote-desk-manager'):
        return patterns
    host = 'desk_signal' if package == 'desk-signal' else 'lcxl_remote_desk_manager::service'
    # Optimized generic polls are inlined into their unchanged caller-owned phases.
    owners = {
        'memory_write::sources::load_on': '^' + host + r'::assistant_data_store::memory_write::write_on::_\{\{closure\}\}$',
        'memory_write::receipts::collect_on': '^' + host + r'::assistant_data_store::output_archive::gc::collect_actor_transaction::_\{\{closure\}\}$',
    }
    return ['(?:' + pattern + '|' + owner + ')' if (owner := next((owner for child, owner in owners.items() if child in pattern), None)) else pattern
            for pattern in patterns]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', type=Path, default=HERE.parents[1])
    parser.add_argument('--package', choices=REQUIRED, required=True)
    parser.add_argument('--profile', choices=['debug', 'release'], default='debug')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    command = [sys.executable, str(HERE / 'check.py'), '--workspace', str(args.workspace),
               '--package', args.package, '--profile', args.profile, '--ceiling', str(report.HARD_FRAME_LIMIT),
               '--output', str(args.output)]
    for pattern in required_patterns(args.package, args.profile):
        command.extend(['--required', pattern])
    return subprocess.call(command)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
