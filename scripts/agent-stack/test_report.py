import json
import re
import unittest
import tempfile
from pathlib import Path
from unittest import mock
import contextlib
import io
import check as stack_check
import report
import run as gate


def record(size, symbol='_ZN18desk_diagnose_core10agent_loop14run_inner_impl17h0123456789abcdefE'):
    return json.dumps({'message': {'message': f"{size} stack bytes in function '{symbol}'"}})


class Reports(unittest.TestCase):
    def test_lease_renewal_requires_the_real_shared_poll(self):
        constructor = 'desk_signal_facade::service::lease_heartbeat::renew_until_lost'
        poll = constructor + '::_{{closure}}::_{{closure}}'
        self.assertTrue(report.protected(poll))
        for package in ('desk-signal', 'lcxl-remote-desk-manager'):
            for profile in ('debug', 'release'):
                patterns = [value for value in gate.required_patterns(package, profile) if 'lease_heartbeat' in value]
                self.assertEqual(len(patterns), 1)
                self.assertEqual(report.check_ceiling({'frames': {poll: 2048}}, report.HARD_FRAME_LIMIT, patterns), [])
                self.assertTrue(report.check_ceiling({'frames': {constructor: 2048}}, report.HARD_FRAME_LIMIT, patterns))
                self.assertTrue(report.check_ceiling({'frames': {poll: report.HARD_FRAME_LIMIT + 1}}, report.HARD_FRAME_LIMIT, patterns))

    def test_encoded_heartbeat_poll_is_collected_before_symbol_normalization(self):
        parts = ['desk_signal_facade', 'service', 'lease_heartbeat', 'renew_until_lost',
                 '_{{closure}}', '_{{closure}}', 'h0123456789abcdef']
        encoded = '_ZN' + ''.join(str(len(part)) + part for part in parts) + 'E'
        data = report.collect([record(report.HARD_FRAME_LIMIT + 1, encoded)])
        poll = 'desk_signal_facade::service::lease_heartbeat::renew_until_lost::_{{closure}}::_{{closure}}'
        self.assertEqual(data['frames'][poll], report.HARD_FRAME_LIMIT + 1)
        self.assertTrue(report.check_ceiling(data, report.HARD_FRAME_LIMIT, []))

    def test_manifest_admission_requires_the_executing_host_trait_future(self):
        for package, store in [
                ('desk-signal', 'desk_signal::agent_session_store::SignalAgentSessionStore'),
                ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service::agent_session_store::AgentSessionStore')]:
            constructor = '_<' + store + ' as desk_diagnose_core::seam::SessionSeam>::assistant_data_request_manifest'
            poll = constructor + '::_{{closure}}'
            parts = store.split('::') + ['assistant_data_request_manifest', 'h0123456789abcdef']
            encoded = '_ZN' + ''.join(str(len(part)) + part for part in parts) + 'E'
            data = report.collect([record(report.HARD_FRAME_LIMIT + 1, encoded)])
            self.assertTrue(report.check_ceiling(data, report.HARD_FRAME_LIMIT, []))
            for profile in ['debug', 'release']:
                patterns = [pattern for pattern in gate.required_patterns(package, profile)
                            if 'assistant_data_request_manifest' in pattern]
                self.assertEqual(len(patterns), 1)
                self.assertEqual(report.check_ceiling({'frames': {poll: 2048}}, report.HARD_FRAME_LIMIT, patterns), [])
                for wrong in [constructor, 'core::ptr::drop_in_place<' + poll + '>']:
                    self.assertTrue(report.check_ceiling({'frames': {wrong: 2048}}, report.HARD_FRAME_LIMIT, patterns))

    def test_recorded_call_restoration_and_receipt_verifiers_keep_the_frame_ceiling(self):
        symbols = [
            'desk_diagnose_core::ui_model_ids::resolve_recorded_call',
            'desk_diagnose_core::schedule::contract::exception::request_for_original_call',
            'desk_diagnose_core::schedule::rehearsal::read_sources::project_tool_result_source',
            'desk_signal::schedule_store::ScheduleStore::read_rehearsal_reads_on',
            'lcxl_remote_desk_manager::service::schedule_store::ScheduleStore::read_rehearsal_read_sources_on',
        ]
        for symbol in symbols:
            with self.subTest(symbol=symbol):
                parts = symbol.split('::') + ['h0123456789abcdef']
                encoded = '_ZN' + ''.join(str(len(part)) + part for part in parts) + 'E'
                data = report.collect([record(report.HARD_FRAME_LIMIT + 1, encoded)])
                self.assertTrue(report.check_ceiling(data, report.HARD_FRAME_LIMIT, []))
        pattern = '^desk_diagnose_core::ui_model_ids::resolve_recorded_call$'
        self.assertIn(pattern, gate.REQUIRED['desk-diagnose-core'])
        self.assertTrue(report.check_ceiling({'frames': {}}, report.HARD_FRAME_LIMIT, [pattern]))

    def test_parameter_validation_execution_frames_are_protected(self):
        for parts in [
            ['desk_agent_protocol','ai_assistant','memory','validate_draft_limits','h0123456789abcdef'],
            ['desk_diagnose_core','model_input','check_at','h0123456789abcdef'],
        ]:
            with self.subTest(parts=parts):
                symbol='_ZN'+''.join(str(len(part))+part for part in parts)+'E'
                data=report.collect([record(report.HARD_FRAME_LIMIT+1,symbol)])
                self.assertTrue(report.check_ceiling(data,report.HARD_FRAME_LIMIT,[]))
        required=gate.required_patterns('desk-agent-protocol','debug')
        self.assertTrue(report.check_ceiling({'frames':{}},report.HARD_FRAME_LIMIT,required))

    def test_parameter_guidance_frames_are_protected_by_the_fixed_ceiling(self):
        parts = ['desk_diagnose_core', 'model_constraints', 'add_byte_budgets', 'h0123456789abcdef']
        symbol = '_ZN' + ''.join(str(len(part)) + part for part in parts) + 'E'
        self.assertTrue(report.protected('desk_diagnose_core::model_constraints::add_byte_budgets'))
        data = report.collect([record(report.HARD_FRAME_LIMIT + 1, symbol)])
        self.assertTrue(report.check_ceiling(data, report.HARD_FRAME_LIMIT, []))

    def test_tool_output_views_require_real_execution_and_the_fixed_frame_ceiling(self):
        symbols = [
            'desk_diagnose_core::ui_model_ids::project_request',
            'desk_diagnose_core::ui_references::UiReferenceState::resolve_conversation_request',
            'desk_diagnose_core::ui_model_ids::project_tool_message',
        ]
        patterns = [pattern for pattern in gate.REQUIRED['desk-diagnose-core']
                    if any(re.search(pattern, symbol) for symbol in symbols)]
        frames = {symbol: 4096 for symbol in symbols}
        self.assertEqual(report.check_ceiling({'frames': frames}, report.HARD_FRAME_LIMIT, patterns), [])
        for symbol in symbols:
            self.assertTrue(report.protected(symbol))
            missing = {name: size for name, size in frames.items() if name != symbol}
            self.assertTrue(report.check_ceiling({'frames': missing}, report.HARD_FRAME_LIMIT, patterns))
            wrong = {name: size for name, size in frames.items() if name != symbol}
            wrong['core::ptr::drop_in_place::' + symbol] = 32
            self.assertTrue(report.check_ceiling({'frames': wrong}, report.HARD_FRAME_LIMIT, patterns))
        frames[symbols[0]] = report.HARD_FRAME_LIMIT + 1
        self.assertTrue(report.check_ceiling({'frames': frames}, report.HARD_FRAME_LIMIT, patterns))

    def test_schedule_argument_receipts_require_execution_and_keep_fixed_limits(self):
        symbols = [
            ('desk-diagnose-core', 'desk_diagnose_core::provider_preflight::read::output_digest'),
            ('desk-diagnose-core', 'desk_diagnose_core::provider_preflight::read::error_output_digest'),
            ('desk-diagnose-core', 'desk_diagnose_core::schedule::rehearsal::read_sources::project_tool_result_source'),
            ('desk-diagnose-core', 'desk_diagnose_core::agent_loop::task_permission::pause::_{{closure}}'),
            ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service::capability_grant_store::read_use::impl::read_use::_{{closure}}'),
        ]
        for package, symbol in symbols:
            patterns = [p for p in gate.required_patterns(package, 'debug') if re.search(p, symbol)]
            self.assertTrue(report.protected(symbol))
            parts = symbol.split('::')
            mangled = '_ZN' + ''.join(str(len(part)) + part for part in parts) + 'E'
            self.assertEqual(report.collect([record(4096, mangled)])['frames'], {symbol: 4096})
            self.assertTrue(patterns)
            self.assertEqual(report.check_ceiling({'frames': {symbol: 4096}}, report.HARD_FRAME_LIMIT, patterns), [])
            self.assertTrue(report.check_ceiling({'frames': {symbol: report.HARD_FRAME_LIMIT + 1}}, report.HARD_FRAME_LIMIT, patterns))
            self.assertTrue(report.check_ceiling({'frames': {'core::ptr::drop_in_place::' + symbol: 32}}, report.HARD_FRAME_LIMIT, patterns))
        self.assertTrue(report.protected('lcxl_remote_desk_manager::service::remote_tool_routing::read_grants::output_digest'))

    def test_release_schedule_error_alias_still_requires_the_real_constructor(self):
        patterns = [p for p in gate.required_patterns('desk-diagnose-core', 'release') if 'proposal::invalid_input' in p]
        self.assertEqual(len(patterns), 1)
        for symbol in ['desk_diagnose_core::schedule::proposal::invalid_input',
                       'desk_diagnose_core::agent_loop::retrieval_accounting::invalid']:
            self.assertEqual(report.check_ceiling({'frames': {symbol: 64}}, report.HARD_FRAME_LIMIT, patterns), [])
            self.assertTrue(report.check_ceiling({'frames': {symbol: report.HARD_FRAME_LIMIT + 1}}, report.HARD_FRAME_LIMIT, patterns))
            self.assertTrue(report.check_ceiling({'frames': {'core::ptr::drop_in_place::' + symbol: 32}}, report.HARD_FRAME_LIMIT, patterns))
        self.assertTrue(report.check_ceiling({'frames': {}}, report.HARD_FRAME_LIMIT, patterns))

    def test_schedule_input_boundaries_require_actual_execution(self):
        symbols = [
            'desk_diagnose_core::ui_model_ids::resolve_session_call_with_context',
            'desk_diagnose_core::schedule::proposal::draft',
            'desk_diagnose_core::schedule::proposal::spec',
            'desk_diagnose_core::schedule::proposal::invalid_input',
            'desk_diagnose_core::schedule::management_tools::parse',
        ]
        for symbol in symbols:
            self.assertTrue(report.protected(symbol))
            required = [p for p in gate.REQUIRED['desk-diagnose-core'] if re.search(p, symbol)]
            self.assertTrue(required)
            self.assertEqual(report.check_ceiling({'frames': {symbol: 4096}}, report.HARD_FRAME_LIMIT, required), [])
            self.assertTrue(report.check_ceiling({'frames': {'core::ptr::drop_in_place<' + symbol + '>': 32}}, report.HARD_FRAME_LIMIT, required))
        for package, host in [('desk-signal', 'desk_signal'), ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
            base = '_<' + host + '::agent_session_store::Store as desk_diagnose_core::seam::SessionSeam>::manage_schedule_tool'
            required = [p for p in gate.REQUIRED[package] if 'manage_schedule_tool' in p]
            self.assertTrue(report.protected(base + '::_{{closure}}'))
            self.assertTrue(report.protected(host + '::schedule_store::proposal::impl::manage_from_session::_{{closure}}'))
            self.assertTrue(report.check_ceiling({'frames': {base: 32}}, report.HARD_FRAME_LIMIT, required))
            self.assertEqual(report.check_ceiling({'frames': {base + '::_{{closure}}': 4096}}, report.HARD_FRAME_LIMIT, required), [])

    def test_conversation_titles_require_real_polls_and_pure_title_logic(self):
        core = 'desk_diagnose_core::agent_loop::title::generate'
        checks = [('desk-diagnose-core', core)]
        for package, host in [('desk-signal', 'desk_signal'), ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
            for method in ('load_title_limit', 'store_title'):
                checks.append((package, host + '::agent_session_store::title::impl::' + method))
        for package, base in checks:
            poll = base + '::_{{closure}}::_{{closure}}'
            patterns = [p for p in gate.REQUIRED[package] if 'title::' in p and ('generate' in p or 'load_title_limit' in p or 'store_title' in p)]
            self.assertTrue(report.protected(poll))
            self.assertTrue(any(re.search(p, poll) for p in patterns))
            parts = poll.split('::')
            mangled = '_ZN' + ''.join(str(len(part)) + part for part in parts) + 'E'
            self.assertEqual(report.collect([record(4096, mangled)])['frames'], {poll: 4096})
            for non_poll in [base, base + '::_{{closure}}', 'core::ptr::drop_in_place::' + poll]:
                self.assertFalse(any(re.search(p, non_poll) for p in patterns))
        for name in ('request', 'parse', 'fallback'):
            symbol = 'desk_diagnose_core::conversation_title::' + name
            self.assertTrue(report.protected(symbol))
            self.assertTrue(any(re.search(p, symbol) for p in gate.REQUIRED['desk-diagnose-core']))

    def test_shared_message_pages_require_execution_frames_and_keep_the_hard_limit(self):
        prefix = 'desk_signal_facade::controller::ai_assistant_session::'
        symbols = [prefix + name for name in (
            'project_snapshot_message_page', 'project_snapshot_message_window',
            'SnapshotMessagePageProjection::with_citations',
        )]
        required = gate.REQUIRED['desk-signal-facade']
        frames = {symbol: 4096 for symbol in symbols}
        self.assertEqual(report.check_ceiling({'frames': frames}, report.HARD_FRAME_LIMIT, required), [])
        for symbol in symbols:
            self.assertTrue(report.protected(symbol))
            missing = {name: size for name, size in frames.items() if name != symbol}
            self.assertTrue(report.check_ceiling({'frames': missing}, report.HARD_FRAME_LIMIT, required))
        frames[symbols[0]] = report.HARD_FRAME_LIMIT + 1
        self.assertTrue(report.check_ceiling({'frames': frames}, report.HARD_FRAME_LIMIT, required))
        self.assertFalse(report.protected('desk_signal_facade::controller::other::render'))

    def test_unsupported_session_retirement_requires_real_execution(self):
        for package, host in [('desk-signal', 'desk_signal'),
                              ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
            for path in ('agent_subagent_store::retention::close_root_for_subject_on',
                         'agent_subagent_store::retention::close_child_on',
                         'agent_session_store::management::retire_unsupported_for_expiry'):
                poll = host + '::' + path + '::_{{closure}}' + ('::_{{closure}}' if 'retire_unsupported' in path or 'close_child_on' in path else '')
                self.assertTrue(report.protected(poll))
                self.assertTrue(any(re.search(pattern, poll) for pattern in gate.REQUIRED[package]))
        self.assertTrue(report.protected('desk_diagnose_core::session::management::SessionManagementMetadata::decode'))

    def test_retirement_mangled_remarks_survive_filtering(self):
        parts = ['desk_signal', 'agent_session_store', 'management', 'retire_unsupported_for_expiry', '_{{closure}}']
        symbol = '_ZN' + ''.join(f'{len(part)}{part}' for part in parts) + '17h0123456789abcdefE'
        data = report.collect([record(4096, symbol)])
        self.assertEqual(data['frames'], {'::'.join(parts): 4096})

    def test_permission_finalization_covers_actual_transaction_poll(self):
        for package, symbol in [
            ('desk-signal', 'desk_signal::agent_session_store::SignalAgentSessionStore::decide_permission_request_with_authority::_{{closure}}'),
            ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service::agent_permission_store::ManagerPermissionStore::decide_with_authority::_{{closure}}'),
        ]:
            self.assertTrue(report.protected(symbol))
            self.assertTrue(any(re.search(pattern, symbol) for pattern in gate.REQUIRED[package]))
        self.assertTrue(report.protected('desk_diagnose_core::permission_grant::apply_automatic_grant_failure'))

    def test_root_wait_scan_requires_its_actual_poll(self):
        for package, host in [('desk-signal', 'desk_signal'),
                              ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
            base = host + '::agent_subagent_store::candidates::_<impl ' + host + '::agent_subagent_store::SubAgentStore>::root_wait_candidates'
            poll = base + '::_{{closure}}::_{{closure}}'
            patterns = [pattern for pattern in gate.REQUIRED[package] if 'root_wait_candidates' in pattern]
            self.assertTrue(report.protected(poll))
            self.assertEqual(report.check_ceiling({'frames': {poll: 4096}}, report.HARD_FRAME_LIMIT, patterns), [])
            for wrong in [base, base + '::_{{closure}}', 'core::ptr::drop_in_place<' + poll + '>']:
                self.assertTrue(report.check_ceiling({'frames': {wrong: 32}}, report.HARD_FRAME_LIMIT, patterns))

    def test_message_page_remarks_survive_the_protected_symbol_filter(self):
        parts = ['desk_signal_facade', 'controller', 'ai_assistant_session', 'project_snapshot_message_page']
        symbol = '_ZN' + ''.join(f'{len(part)}{part}' for part in parts) + '17h0123456789abcdefE'
        data = report.collect([record(4096, symbol)])
        self.assertEqual(data['frames'], {'::'.join(parts): 4096})

    def test_direct_memory_writes_require_real_polls_and_keep_physical_identity(self):
        for package, host in [('desk-signal', 'desk_signal'),
                              ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service')]:
            constructor = host + '::assistant_data_store::memory_write::write_on'
            poll = constructor + '::_{{closure}}'
            patterns = [pattern for pattern in gate.REQUIRED[package] if 'memory_write::write_on' in pattern]
            self.assertEqual(len(patterns), 1)
            data = report.retain_operation_identity({'frames': {poll: 4096}}, package)
            self.assertEqual(data['frames'], {poll: 4096})
            self.assertEqual(report.check_ceiling(data, report.HARD_FRAME_LIMIT, patterns), [])
            self.assertTrue(report.check_ceiling({'frames': {constructor: 64}}, report.HARD_FRAME_LIMIT, patterns))

    def test_native_archive_reader_stays_protected_and_requires_its_poll(self):
        for package, symbol in [('desk-signal', 'desk_signal::model_egress_store::SignalModelEgressStore::read_context_output_with_archive_on'),
                                ('lcxl-remote-desk-manager', 'lcxl_remote_desk_manager::service::ai_call_request::read_context_output_with_archive_on')]:
            poll = symbol + '::_{{closure}}'
            patterns = [pattern for pattern in gate.REQUIRED[package] if 'read_context_output_with_archive_on' in pattern]
            self.assertTrue(report.protected(poll))
            self.assertEqual(report.check_ceiling({'frames':{poll:4096}},report.HARD_FRAME_LIMIT,patterns),[])
            self.assertTrue(report.check_ceiling({'frames':{symbol:64}},report.HARD_FRAME_LIMIT,patterns))

    def test_optimized_memory_helpers_require_the_real_owner_poll(self):
        for package, host in [('desk-signal','desk_signal'), ('lcxl-remote-desk-manager','lcxl_remote_desk_manager::service')]:
            for child, owner in [('memory_write::sources::load_on','memory_write::write_on'),
                                 ('memory_write::receipts::collect_on','output_archive::gc::collect_actor_transaction')]:
                original = '^desk_assistant_data_store::' + child + r'::_\{\{closure\}\}$'
                constructor = host + '::assistant_data_store::' + owner
                poll = constructor + '::_{{closure}}'
                release = [pattern for pattern in gate.required_patterns(package,'release') if child in pattern]
                self.assertEqual(len(release),1)
                self.assertEqual(report.check_ceiling({'frames':{poll:4096}}, report.HARD_FRAME_LIMIT, release),[])
                self.assertTrue(report.check_ceiling({'frames':{poll:report.HARD_FRAME_LIMIT+1}}, report.HARD_FRAME_LIMIT,release))
                for wrong in [constructor,'core::ptr::drop_in_place<'+poll+'>']:
                    self.assertTrue(report.check_ceiling({'frames':{wrong:64}},report.HARD_FRAME_LIMIT,release))
                self.assertIn(original,gate.required_patterns(package,'debug'))
                self.assertTrue(report.check_ceiling({'frames':{poll:4096}},report.HARD_FRAME_LIMIT,[original]))

    def test_entity_relocation_retains_original_report_identity(self):
        old = 'core::ptr::drop_in_place<desk_database::entity::agent_assistant_model_output::Model>'
        actual = old.replace('desk_database::entity::', 'desk_assistant_data_store::entity::')
        data = report.retain_entity_identity({'frames': {actual: 64}}, 'lcxl-remote-desk-manager')
        self.assertEqual(data['frames'], {old: 64})
        self.assertEqual(data['entity_symbol_relocations'], {old: actual})
        self.assertEqual(report.check_ceiling(data, report.HARD_FRAME_LIMIT, []), [])
        with self.assertRaisesRegex(ValueError, 'two physical owners'):
            report.retain_entity_identity({'frames': {old: 32, actual: 32}}, 'lcxl-remote-desk-manager')

    def test_signal_native_quota_extension_is_not_aliased_to_the_shared_counter_view(self):
        native = 'core::ptr::drop_in_place<desk_signal::entity::agent_assistant_quota::Model>'
        common = native.replace('desk_signal::entity::', 'desk_assistant_data_store::entity::')
        data = report.retain_entity_identity({'frames': {native: 64, common: 32}}, 'desk-signal')
        self.assertEqual(data['frames'], {common: 32, native: 64})


    def test_shared_storage_requires_emitted_snapshot_decoders_and_keeps_hard_ceiling(self):
        required = gate.REQUIRED['desk-assistant-data-store']
        frames = {pattern.removeprefix('^').removesuffix('$'): 4096 for pattern in required}
        self.assertEqual(report.check_ceiling({'frames': frames}, report.HARD_FRAME_LIMIT, required), [])
        for symbol in frames:
            self.assertTrue(report.protected(symbol))
            missing = {key: value for key, value in frames.items() if key != symbol}
            self.assertTrue(report.check_ceiling({'frames': missing}, report.HARD_FRAME_LIMIT, required))
        overflow = dict(frames)
        overflow['desk_assistant_data_store::memory_index::prepare_on::_{{closure}}'] = report.HARD_FRAME_LIMIT + 1
        self.assertTrue(report.check_ceiling({'frames': overflow}, report.HARD_FRAME_LIMIT, required))

    def test_published_prompt_requires_original_contract_and_actual_host_polls(self):
        for name in ['TaskPromptRetention::validate', 'is_published_input_label']:
            symbol = 'desk_diagnose_core::schedule::published_input::' + name
            patterns = [pattern for pattern in gate.REQUIRED['desk-diagnose-core'] if pattern == '^' + symbol + '$']
            self.assertEqual(len(patterns), 1)
            self.assertTrue(report.protected(symbol))
            self.assertEqual(report.check_ceiling({'frames': {symbol: 128}}, report.HARD_FRAME_LIMIT, patterns), [])
        for package in ['desk-signal', 'lcxl-remote-desk-manager']:
            patterns = [pattern for pattern in gate.REQUIRED[package] if any(name in pattern for name in [
                'owner_prompt_retention_on', 'record_prompt_retention_on', 'published_input_retention_on', 'native::inputs::published_on'])]
            self.assertEqual(len(patterns), 4)
            for pattern in patterns:
                constructor = pattern.removeprefix('^').split('.*', 1)[0].rstrip(':')
                poll = constructor + '::_{{closure}}' * pattern.count('closure')
                self.assertTrue(report.protected(poll))
                self.assertTrue(report.check_ceiling({'frames': {constructor: 128}}, report.HARD_FRAME_LIMIT, [pattern]))
                self.assertEqual(report.check_ceiling({'frames': {poll: 256}}, report.HARD_FRAME_LIMIT, [pattern]), [])


    def test_snapshots_require_frozen_policy_and_adoption_polls(self):
        for name in ['read_projection::ReadProjectionProof::snapshot_envelope',
                     'seam::AssistantModelDataManifest::validate_for']:
            symbol = 'desk_diagnose_core::ai_assistant::' + name
            patterns = [p for p in gate.REQUIRED['desk-diagnose-core'] if p == '^' + symbol + '$']
            self.assertEqual(len(patterns), 1)
            self.assertTrue(report.protected(symbol))
            self.assertEqual(report.check_ceiling({'frames': {symbol: 512}}, report.HARD_FRAME_LIMIT, patterns), [])
            self.assertTrue(report.check_ceiling({'frames': {'core::ptr::drop_in_place<' + symbol + '>': 32}}, report.HARD_FRAME_LIMIT, patterns))
        for package in ['desk-signal', 'lcxl-remote-desk-manager']:
            self.assertTrue(any('visible_history::snapshot::prepare_on' in p for p in gate.REQUIRED[package]))
            self.assertTrue(any('visible_history::native::resolve_policy_on' in p for p in gate.REQUIRED[package]))

    def test_history_readers_require_actual_polls(self):
        for package in ['desk-signal', 'lcxl-remote-desk-manager']:
            prefix = 'desk_signal' if package == 'desk-signal' else 'lcxl_remote_desk_manager::service'
            for name in ['owner_source::history_source_on', 'model_sources::validate_history_on']:
                constructor = prefix + '::assistant_data_store::' + name
                patterns = [pattern for pattern in gate.REQUIRED[package] if name in pattern]
                self.assertEqual(len(patterns), 1)
                poll = constructor + '::_{{closure}}'
                self.assertTrue(report.protected(poll))
                self.assertEqual(report.check_ceiling({'frames': {poll: 16384}}, report.HARD_FRAME_LIMIT, patterns), [])
                for wrong in [constructor, 'core::ptr::drop_in_place<' + poll + '>']:
                    self.assertTrue(report.check_ceiling({'frames': {wrong: 32}}, report.HARD_FRAME_LIMIT, patterns))



    def test_expired_fresh_context_recovery_requires_the_native_poll(self):
        for package in ['desk-signal', 'lcxl-remote-desk-manager']:
            patterns = [pattern for pattern in gate.REQUIRED[package]
                        if 'recover_action_free_fresh_task' in pattern]
            self.assertEqual(len(patterns), 1)
            prefix = 'desk_signal' if package == 'desk-signal' else 'lcxl_remote_desk_manager::service'
            constructor = prefix + '::schedule_store::fresh_recovery::_<impl ' + prefix + '::schedule_store::ScheduleStore>::recover_action_free_fresh_task'
            poll = constructor + '::_{{closure}}::_{{closure}}'
            self.assertTrue(report.protected(poll))
            self.assertTrue(report.check_ceiling({'frames': {constructor: 128}}, report.HARD_FRAME_LIMIT, patterns))
            self.assertTrue(report.check_ceiling({'frames': {constructor + '::_{{closure}}': 128}}, report.HARD_FRAME_LIMIT, patterns))
            self.assertTrue(report.check_ceiling({'frames': {'core::ptr::drop_in_place<' + poll + '>': 64}}, report.HARD_FRAME_LIMIT, patterns))
            self.assertEqual(report.check_ceiling({'frames': {poll: 65536}}, report.HARD_FRAME_LIMIT, patterns), [])
            for name in ['reconcile_expired_fresh_task', 'settle_expired_fresh_task']:
                phases = [pattern for pattern in gate.REQUIRED[package] if name in pattern]
                self.assertEqual(len(phases), 1)
                phase = constructor.rsplit('::', 1)[0] + '::' + name
                self.assertTrue(report.protected(phase))
                self.assertTrue(report.check_ceiling({'frames': {phase: 128}}, report.HARD_FRAME_LIMIT, phases))
                self.assertEqual(report.check_ceiling({'frames': {phase + '::_{{closure}}': 65536}}, report.HARD_FRAME_LIMIT, phases), [])
            for module in ['fresh_children_recovery', 'fresh_permission_recovery']:
                phases = [pattern for pattern in gate.REQUIRED[package] if 'schedule_store::' + module in pattern]
                self.assertEqual(len(phases), 1)
                phase = prefix + '::schedule_store::' + module + '::recover'
                self.assertTrue(report.protected(phase))
                self.assertTrue(report.check_ceiling({'frames': {phase: 128}}, report.HARD_FRAME_LIMIT, phases))
                self.assertEqual(report.check_ceiling({'frames': {phase + '::_{{closure}}': 65536}}, report.HARD_FRAME_LIMIT, phases), [])

    def test_fresh_task_entry_requires_its_actual_shared_poll_in_each_host(self):
        for package in ['desk-signal', 'lcxl-remote-desk-manager']:
            patterns = [pattern for pattern in gate.REQUIRED[package]
                        if 'fresh_task::resume_claimed_fresh_task_turn' in pattern]
            self.assertEqual(len(patterns), 1)
            constructor = 'desk_diagnose_core::agent_loop::fresh_task::resume_claimed_fresh_task_turn'
            poll = constructor + '::_{{closure}}'
            self.assertTrue(report.check_ceiling({'frames': {constructor: 128}}, report.HARD_FRAME_LIMIT, patterns))
            self.assertTrue(report.check_ceiling({'frames': {'core::ptr::drop_in_place<' + poll + '>': 64}}, report.HARD_FRAME_LIMIT, patterns))
            self.assertEqual(report.check_ceiling({'frames': {poll: 19632}}, report.HARD_FRAME_LIMIT, patterns), [])

    def test_metadata_required_poll_frames_reject_constructors_and_drop(self):
        for package in ['desk-signal', 'lcxl-remote-desk-manager']:
            patterns = [pattern for pattern in gate.REQUIRED[package]
                        if 'closure' in pattern and any(name in pattern for name in [
                            'assistant_data_store::', 'schedule_store::', 'agent_goal_store::claim_slice',
                            'agent_run_event_store::', 'agent_subagent_store::observation::'])]
            self.assertEqual(len(patterns), 72 if package == 'desk-signal' else 74)
            for pattern in patterns:
                constructor = pattern.removeprefix('^').split('.*', 1)[0].split(r'::_\{', 1)[0].rstrip(':')
                depth = 2 if pattern.count('closure') == 2 else 1
                poll = constructor + '::_{{closure}}' * depth
                with self.subTest(package=package, pattern=pattern):
                    self.assertTrue(report.check_ceiling({'frames': {constructor: 48}}, report.HARD_FRAME_LIMIT, [pattern]))
                    self.assertTrue(report.check_ceiling({'frames': {'core::ptr::drop_in_place<' + poll + '>': 64}}, report.HARD_FRAME_LIMIT, [pattern]))
                    if depth == 2:
                        self.assertTrue(report.check_ceiling({'frames': {constructor + '::_{{closure}}': 16}}, report.HARD_FRAME_LIMIT, [pattern]))
                    self.assertEqual(report.check_ceiling({'frames': {poll: 65536}}, report.HARD_FRAME_LIMIT, [pattern]), [])

    def test_memory_stores_and_shared_contracts_enforce_the_same_ceiling(self):
        for parts in [
            ['desk_diagnose_core', 'ai_assistant', 'data_control', 'ProjectionRecovery', 'accept_owner_input'],
            ['desk_signal', 'assistant_data_store', 'memory_write', 'write_on'],
            ['desk_signal', 'agent_run_event_store', 'SignalAgentRunEventStore', 'handled_outcome'],
            ['lcxl_remote_desk_manager', 'service', 'agent_run_event_store', 'ManagerRunEventStore', 'read_user_input_state'],
            ['lcxl_remote_desk_manager', 'service', 'assistant_data_store', 'memory_write', 'write_on'],
            ['lcxl_remote_desk_manager', 'service', 'agent_goal_store', 'settle_slice_afterimage'],
            ['desk_signal', 'agent_goal_store', 'settle_slice'],
            ['desk_signal', 'agent_goal_store', 'persist_slice_on'],
            ['desk_signal', 'agent_goal_store', 'validate_slice_session_on'],
            ['desk_signal', 'agent_goal_store', 'claim_slice'],
            ['desk_signal', 'schedule_store', 'ScheduleStore', 'capture_retrieval_consent_on'],
            ['desk_signal', 'schedule_store', 'ScheduleStore', 'capture_definition_consent_on'],
            ['desk_signal', 'schedule_store', 'fresh_session', 'ScheduleStore', 'insert_fresh_session_inner'],
            ['desk_signal', 'schedule_store', 'context_wait', 'ScheduleStore', 'settle_context_wait_inner'],
            ['lcxl_remote_desk_manager', 'service', 'schedule_store', 'context_wait', 'ScheduleStore', 'lock_context_pause_on'],
            ['lcxl_remote_desk_manager', 'service', 'schedule_store', 'fresh_session', 'ScheduleStore', 'initialize_fresh_assistant_on'],
            ['desk_signal', 'schedule_store', 'resume_admission', 'ScheduleStore', 'assistant_retrieval_deadline_on'],
            ['lcxl_remote_desk_manager', 'service', 'schedule_store', 'resume_claim', 'ScheduleStore', 'claim_continuation_on'],
            ['lcxl_remote_desk_manager', 'service', 'schedule_store', 'ScheduleStore', 'create_draft_on'],
            ['lcxl_remote_desk_manager', 'service', 'agent_goal_store', 'scheduler_transition'],
            ['desk_signal', 'schedule_executor', 'SignalScheduleExecutor', 'process_goal'],
            ['lcxl_remote_desk_manager', 'service', 'schedule_executor', 'goal', 'ScheduleExecutor', 'scan_goals_once'],
            ['desk_diagnose_core', 'ai_assistant', 'memory_evidence', 'MemoryEvidenceRetentionProof', 'from_snapshot'],
            ['desk_diagnose_core', 'model_context', 'checkpoint', 'ContextProtectionSet', 'validate_references'],
            ['desk_diagnose_core', 'goal', 'GoalRun', 'finish_assistant_context_wait_slice'],
            ['desk_diagnose_core', 'schedule', 'context_wait', 'finish'],
            ['desk_diagnose_core', 'schedule', 'context_wait', 'claim'],
            ['desk_diagnose_core', 'ai_assistant', 'data_control', 'memory_forget', 'commit_memory_forget_group'],
            ['desk_diagnose_core', 'ai_assistant', 'data_control', 'memory_forget', 'read_memory_forget_group'],
        ]:
            with self.subTest(parts=parts):
                symbol = '_ZN' + ''.join(f'{len(part)}{part}' for part in parts) + '17h0123456789abcdefE'
                result = report.collect([record(report.HARD_FRAME_LIMIT + 1, symbol)])
                self.assertEqual(len(result['frames']), 1)
                self.assertTrue(report.check_ceiling(result, report.HARD_FRAME_LIMIT, []))

    def test_portable_ceiling_also_requires_its_critical_families(self):
        result = report.collect([record(65537)])
        self.assertTrue(report.check_ceiling(result, 65536, ['run_inner_impl']))
        self.assertTrue(report.check_ceiling(result, 131072, ['missing']))
        self.assertEqual(report.check_ceiling(result, 131072, ['run_inner_impl']), [])

    def test_llvm_optimized_symbol_suffix(self):
        symbol = '_ZN18desk_diagnose_core10agent_loop14run_inner_impl17h0123456789abcdefE.llvm.123456789'
        self.assertEqual(report.collect([record(32, symbol)])['frames'],
                         {'desk_diagnose_core::agent_loop::run_inner_impl': 32})

    def test_raw_rustc_diagnostics_are_parsed(self):
        diagnostic = json.loads(record(131136))['message']
        self.assertEqual(report.collect([json.dumps(diagnostic)])['records'], 1)

    def test_large_frames_and_duplicate_monomorphizations(self):
        result = report.collect([record(64), 'not JSON', record(131136)])
        self.assertEqual(result['records'], 2)
        self.assertEqual(list(result['frames'].values()), [131136])
        self.assertEqual(list(result['frames']), ['desk_diagnose_core::agent_loop::run_inner_impl'])

    def test_empty_cached_or_changed_format_fails_closed(self):
        for lines in ([], ['{"reason":"compiler-artifact","fresh":true}'], ['{"message":"unknown format"}']):
            with self.assertRaises(ValueError):
                report.collect(lines)

    def test_unknown_symbol_format_fails_closed(self):
        with self.assertRaises(ValueError):
            report.collect([record(123, '_Ragent_loop')])

    def test_growth_below_hard_limit_needs_no_historical_allowance(self):
        for size in [32, 4096, report.HARD_FRAME_LIMIT]:
            self.assertEqual(report.check_ceiling({'frames': {'current::poll': size}}, report.HARD_FRAME_LIMIT, []), [])
        self.assertTrue(report.check_ceiling({'frames': {'current::poll': report.HARD_FRAME_LIMIT + 1}}, report.HARD_FRAME_LIMIT, []))

    def test_gate_does_not_load_or_compare_historical_budgets(self):
        with tempfile.TemporaryDirectory() as directory:
            argv = ['run.py', '--workspace', directory, '--package', 'desk-diagnose-core',
                    '--output', str(Path(directory) / 'frames.json')]
            with mock.patch('sys.argv', argv), mock.patch.object(gate.subprocess, 'call', return_value=0) as call, \
                 mock.patch.object(gate.subprocess, 'check_output', side_effect=AssertionError('historical lookup')):
                self.assertEqual(gate.main(), 0)
            command = call.call_args.args[0]
            self.assertNotIn('--baseline', command)
            self.assertNotIn('--previous-baseline', command)
            self.assertEqual(command[command.index('--ceiling') + 1], str(report.HARD_FRAME_LIMIT))

    def test_checker_enforces_hard_limit_by_default(self):
        for size, expected in [(report.HARD_FRAME_LIMIT, 0), (report.HARD_FRAME_LIMIT + 1, 1)]:
            with self.subTest(size=size), tempfile.TemporaryDirectory() as directory:
                argv = ['check.py', '--workspace', directory, '--package', 'desk-diagnose-core',
                        '--output', str(Path(directory) / 'frames.json')]
                with mock.patch('sys.argv', argv), \
                     mock.patch.object(stack_check.subprocess, 'check_output', return_value='rustc test\nhost: test'), \
                     mock.patch.object(stack_check, 'known_overflow_probe'), \
                     mock.patch.object(stack_check, 'collect_package', return_value={'records': 1, 'frames': {'current::poll': size}}), \
                     mock.patch.object(stack_check, 'workspace_identity', return_value={}), \
                     mock.patch.object(stack_check, 'build_configuration', return_value={}), \
                     contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(stack_check.main(), expected)


if __name__ == '__main__':
    unittest.main()
