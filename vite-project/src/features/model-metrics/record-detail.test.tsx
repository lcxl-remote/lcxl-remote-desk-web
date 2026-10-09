import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import type { ObservationRecord } from '@/services/types';
import { ObservationDetails } from './record-detail';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((module) => module.reactI18nextMock()));
afterEach(cleanup);

function input(): ObservationRecord {
    return {
        id: 'root.tool.0', call_id: 'root', tool_observation_id: null, kind: 'tool',
        started_at: '2026-10-08T00:00:00Z', updated_at: '2026-10-08T00:00:01Z',
        provider_id: 'provider', model_id: 'model', model_name: 'Selected model',
        purpose: 'agent', surface: 'assistant', origin: 'user', configuration_scope: 'local',
        configuration_revision: '1', contract_revision: '1', protocol: 'open_ai_chat_completions',
        outcome: 'rejected', not_started_reason: null, output: null, tool: 'request_permissions', ordinal: 0,
        input_conclusion: 'rejected', input_issue: 'type', schema_path: null,
        stages: [], permission: 'denied', correction_of: null, correction_status: 'linked', correction_input: 'rejected',
        correction_group_root: 'root.tool.0', correction_group_unavailable: false,
        correction_group: { root_id: 'root.tool.0', last_input_id: 'last.tool.0', category: 'approval', reason: null,
            outcome: 'input_accepted', linked_attempts: '2', updated_at: '2026-10-08T00:02:00Z' },
        duration_ms: null, headers_ms: null, first_content_ms: null, http_status: null,
        input_tokens: null, output_tokens: null, cache_read_tokens: null, cache_write_tokens: null,
        usage_complete: null, generated_tool_count: null, tool_count: null, input_rejected_count: null, tool_counts_status: 'not_applicable', dispatched: null, detail_trimmed: false,
    };
}

describe('observed correction group details', () => {
    it('keeps long internal correlation identities out of display while retaining the correct detail link', () => {
        const record = { ...input(), call_id: 'a'.repeat(64), correction_group: null };
        const select = vi.fn();
        render(<ObservationDetails record={record} expanded select={select}/>);
        expect(screen.queryByText('a'.repeat(64))).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'aaaaaaaaaaaa…aaaaaaaa' }));
        expect(select).toHaveBeenCalledWith('a'.repeat(64));
    });

    it('shows unknown model identity and the actual unsent reason without a guessed model', () => {
        const record: ObservationRecord = { ...input(), kind: 'call', model_id: '', model_name: '', provider_id: '',
            configuration_revision: '', configuration_scope: 'unknown', protocol: 'unknown',
            outcome: 'not_started', not_started_reason: 'request_validation', tool: null,
            correction_group: null, correction_group_root: null, input_conclusion: null, input_issue: null,
            correction_status: null, correction_input: null, permission: null, ordinal: null };
        render(<ObservationDetails record={record} expanded select={vi.fn()}/>);
        expect(screen.getByText('Reason no request was sent')).toBeTruthy();
        expect(screen.getByText('Request validation rejected')).toBeTruthy();
        expect(screen.getAllByText('Not observed').length).toBeGreaterThan(0);
        expect(screen.queryByText('Selected model')).toBeNull();
    });
    it('names a checked protocol response independently from tool-input and owner approval', () => {
        const record: ObservationRecord = { ...input(), id: 'source-call', kind: 'call', outcome: 'returned',
            correction_group_root: 'source-call', correction_group: { root_id: 'source-call', last_input_id: 'next-call',
                category: 'approval', reason: 'permission_plan_protocol', outcome: 'output_accepted', linked_attempts: '1', updated_at: '2026-10-08T00:02:00Z' } };
        const select = vi.fn();
        render(<ObservationDetails record={record} expanded select={select}/>);
        expect(screen.getByText('Permission-plan protocol correction')).toBeTruthy();
        expect(screen.getByText('Subsequent output passed the recovery check')).toBeTruthy();
        expect(screen.getByText('Projected subsequent requests')).toBeTruthy();
        expect(screen.getByText(/does not prove tool-input acceptance, permission approval or execution/)).toBeTruthy();
        expect(screen.queryByText('Latest linked input accepted')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'next-call' }));
        expect(select).toHaveBeenCalledWith('next-call');
    });
    it('shows the final group verdict separately from an edge verdict and owner decision', () => {
        const select = vi.fn();
        render(<ObservationDetails record={input()} expanded select={select}/>);
        expect(screen.getByRole('region', { name: 'Observed correction group' })).toBeTruthy();
        expect(screen.getByText('Approval-request input')).toBeTruthy();
        expect(screen.getByText('Latest linked input accepted')).toBeTruthy();
        expect(screen.getByText('2')).toBeTruthy();
        expect(screen.queryByText('[object Object]')).toBeNull();
        expect(screen.getByText(/Input acceptance does not prove owner approval/)).toBeTruthy();
        fireEvent.click(screen.getByRole('button', { name: 'last.tool.0' }));
        expect(select).toHaveBeenCalledWith('last.tool.0');
    });

    it('links a member to its original root without manufacturing another group summary', () => {
        const record = { ...input(), id: 'member.tool.0', correction_group: null };
        const select = vi.fn();
        render(<ObservationDetails record={record} expanded select={select}/>);
        expect(screen.queryByRole('region', { name: 'Observed correction group' })).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'root.tool.0' }));
        expect(select).toHaveBeenCalledWith('root.tool.0');
    });

    it('shows unavailable retained facts without a failed or accepted group conclusion', () => {
        render(<ObservationDetails record={{ ...input(), correction_group: null, correction_group_root: null, correction_group_unavailable: true }} expanded select={vi.fn()}/>);
        expect(screen.getByText(/Missing, frozen or trimmed facts are not treated as a failed correction/)).toBeTruthy();
        expect(screen.queryByText('Latest linked input accepted')).toBeNull();
        expect(screen.queryByText('Latest linked input rejected')).toBeNull();
    });
});
