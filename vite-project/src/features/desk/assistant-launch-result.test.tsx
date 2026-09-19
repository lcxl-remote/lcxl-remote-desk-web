import { render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantLaunchResult, parseLaunchReceipt } from './assistant-launch-result';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const receipt = { launch_outcome: 'launch_accepted', argument_delivery: 'submitted', requested_admin: false,
    created_process_id: 42, created_process_elevated: false, failure_reason: null };
const completion = { result: 'changed_but_unverified', output: { kind: 'application_launch', value: receipt } };
describe('native launch result', () => {
    it('renders native failure details as plain text', () => {
        const value = { ...receipt, launch_outcome: 'launch_failed', created_process_id: null, created_process_elevated: null, diagnostic: { stage: 'process_creation', operation: 'CreateProcessW', domain: 'win32', code: 740, message: '<img src=x> elevation required' } };
        const text = JSON.stringify({ result: 'definitely_not_started', output: { kind: 'application_launch', value } });
        const parsed = parseLaunchReceipt(text)!;
        expect(parsed).not.toBeNull();
        const { container } = render(<AssistantLaunchResult receipt={parsed} text={text} />);
        expect(container.textContent).toContain('740');
        expect(container.textContent).toContain('<img src=x> elevation required');
        expect(container.querySelector('img')).toBeNull();
    });
    it('rejects contradictory privilege, process and acceptance facts', () => {
        for (const fields of [{ created_process_elevated: true }, { created_process_id: 4294967296 },
            { failure_reason: 'native_failure' }, { argument_delivery: 'unsupported' }]) {
            expect(parseLaunchReceipt(JSON.stringify({ ...completion, output: {
                kind: 'application_launch', value: { ...receipt, ...fields },
            } }))).toBeNull();
        }
    });
    it('does not accept a receipt claiming verified readiness', () => {
        expect(parseLaunchReceipt(JSON.stringify(completion))).not.toBeNull();
        expect(parseLaunchReceipt(JSON.stringify({ ...completion, result: 'verified' }))).toBeNull();
        expect(parseLaunchReceipt(JSON.stringify({ ...completion, output: { kind: 'application_launch', value: { ...receipt, created_process_id: 0 } } }))).toBeNull();
    });
    it('keeps unknown outcomes visible and asks for observation before retry', () => {
        const text = JSON.stringify({ result: 'outcome_unknown', output: { kind: 'application_launch', value: {
            ...receipt, launch_outcome: 'outcome_unknown', argument_delivery: 'unknown', created_process_id: null, created_process_elevated: null,
        } } });
        const parsed = parseLaunchReceipt(text)!;
        expect(parsed.launch_outcome).toBe('outcome_unknown');
        const { container } = render(<AssistantLaunchResult receipt={parsed} text={text} />);
        expect(container.textContent).toContain('launchReceipt.noRetry');
        expect(container.textContent).toContain('launchReceipt.readiness');
    });
});
