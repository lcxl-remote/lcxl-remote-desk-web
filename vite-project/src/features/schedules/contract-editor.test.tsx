import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { ScheduleManagementResponse } from '@/services/types';
import { ContractEditor } from './contract-editor';
import { ScheduleClient } from './client';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

describe('contract destination identity', () => {
    it('accepts reordered equivalent destinations and still rejects a real scope change', async () => {
        const destination = { kind: 'model', connection_id: 'connection', connection_revision: 1, model_id: 'model', profile_revision: 1 };
        const reordered = { model_id: 'model', kind: 'model', profile_revision: 1, connection_revision: 1, connection_id: 'connection' };
        const limits = { max_calls: 1, max_bytes_per_call: 1024, max_items_per_call: 1 };
        const scope = { resources: ['device:1'], operations: ['read'], export_destinations: [destination], limits };
        const review = { result: 'task_contract', task: { schedule_id: 'task-1', revision: 1 }, contract: {
            schema_version: 1, schedule_id: 'task-1', task_revision: 1, contract_revision: 1, target_device_id: 'device-1', prompt_sha256: 'a'.repeat(64),
            budget: { max_runs_per_utc_day: 4, max_calls_per_run: 10, max_model_tokens_per_run: 1000, max_runtime_seconds: 60 },
            exception_mode: 'request_approval', steps: [], permissions: [{ rule_id: 'read', tool_name: 'read_system_info', input: { kind: 'exact', canonical_json: '{}' },
                automatic: scope, approval_ceiling: { ...scope, export_destinations: [reordered] } }],
        } } as unknown as Extract<ScheduleManagementResponse, { result: 'task_contract' }>;
        const request = vi.fn().mockResolvedValue(review);
        const saved = vi.fn();
        render(<ContractEditor client={{ request } as unknown as ScheduleClient} review={review} connected onSaved={saved} onClose={vi.fn()} />);
        const save = screen.getByRole('button', { name: 'Save new contract version' });
        expect(save).toBeEnabled();
        expect(screen.queryByRole('alert')).not.toBeInTheDocument();
        const checks = screen.getAllByRole('checkbox');
        const ceiling = checks[checks.length - 1];
        fireEvent.click(ceiling);
        expect(save).toBeDisabled();
        expect(screen.getByRole('alert')).toBeInTheDocument();
        fireEvent.click(ceiling);
        expect(save).toBeEnabled();
        fireEvent.click(save);
        await waitFor(() => expect(saved).toHaveBeenCalledWith(review));
        expect(request).toHaveBeenCalledWith({ operation: 'save_task_contract', expected_revision: 1, contract: review.contract });
    });
});
