import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ApprovalModelPublic, ApprovalModelUpdate, ModelProviderPublic } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import { RestResponseError } from '@/lib/kubb-client';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const h = vi.hoisted(() => ({ get: vi.fn(), update: vi.fn(), test: vi.fn(), gateway: vi.fn(), reuse: vi.fn(), toast: vi.fn() }));
vi.mock('@/hooks/use-toast', () => ({ useToast: () => ({ toast: h.toast }) }));
vi.mock('@/services/clients', () => ({
    getApprovalModelProvider: h.get,
    updateApprovalModelProvider: h.update,
    testApprovalModelProvider: h.test,
    getModelProvider: h.gateway,
    reuseAiGatewayForApproval: h.reuse,
}));
import { ApprovalModelSettings } from './approval-model-settings';

let config: ApprovalModelPublic;
let gateway: ModelProviderPublic;
beforeEach(() => {
    vi.resetAllMocks();
    config = {
        enabled: true, api_key_set: true, available: false, unavailable_reason: 'approval_model_not_tested',
        configuration_revision: 7, connection_revision: 3, profile_revision: 4,
        wire_protocol: 'open_ai_chat_completions', model: 'review-model', base_url: 'https://example.test/v1',
        request_options: {}, output_limit_field: 'max_tokens',
        runtime_max_output_tokens: 4096, max_context_bytes: 131072,
    };
    h.get.mockImplementation(async () => ({ success: true, data: config }));
    gateway = {
        ...config, model: 'main-model', connection_revision: 41, profile_revision: 17,
        request_options: { reasoning_effort: 'high' }, runtime_max_output_tokens: 8192,
        execution_mode: 'confirm_each_action', supports_image_input: true, response_format: 'json_object',
        profile_schema_version: 2, max_steps_per_turn: 10, max_same_tool_calls_per_turn: 3,
        exec_approval_timeout_secs: 300,
    };
    h.gateway.mockImplementation(async () => ({ success: true, data: gateway }));
    h.reuse.mockImplementation(async () => ({ success: true, data: {
        ...config, wire_protocol: gateway.wire_protocol, model: gateway.model, base_url: gateway.base_url, api_key_set: true,
        request_options: gateway.request_options, runtime_max_output_tokens: gateway.runtime_max_output_tokens,
        output_limit_field: gateway.output_limit_field,
        max_context_bytes: gateway.max_context_bytes,
        configuration_revision: 8, connection_revision: 5, profile_revision: 6,
    } }));
    h.update.mockImplementation(async (payload: ApprovalModelUpdate) => ({ success: true, data: {
        ...config, ...payload, configuration_revision: 8, connection_revision: 5, profile_revision: 6,
        api_key_set: payload.api_key === '' ? false : true,
    } }));
    h.test.mockResolvedValue({ success: true, data: { saved_as_current: true, latency_ms: 120, validated_capabilities: ['allow', 'deny'] } });
});

function reuseButton() { return screen.getByRole('button', { name: 'Reuse AI gateway configuration' }); }
async function requestReuse() {
    await waitFor(() => expect(reuseButton()).toBeEnabled());
    fireEvent.click(reuseButton());
}

async function expectToast(description: string) {
    await waitFor(() => expect(h.toast).toHaveBeenLastCalledWith(expect.objectContaining({
        description: expect.stringContaining(description),
    })));
}

describe('explicitly reusing the saved AI gateway', () => {
    it('copies an empty approval configuration immediately without secrets or an automatic probe', async () => {
        config = { ...config, enabled: false, model: null, base_url: null, wire_protocol: null, api_key_set: false };
        render(<ApprovalModelSettings />);
        await requestReuse();
        await screen.findByDisplayValue('main-model');
        expect(h.reuse).toHaveBeenCalledExactlyOnceWith({ expected_configuration_revision: 7,
            expected_connection_revision: 3, expected_profile_revision: 4 });
        expect(screen.queryByRole('alertdialog')).toBeNull();
        expect(screen.getByRole('switch', { name: 'Enable approval model' })).not.toBeChecked();
        expect(screen.getByLabelText('API Key')).toHaveValue('');
        expect(screen.getByRole('button', { name: 'Save', exact: true })).toBeDisabled();
        expect(h.update).not.toHaveBeenCalled();
        expect(h.test).not.toHaveBeenCalled();
        await waitFor(() => expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled());
        config = { ...config, wire_protocol: gateway.wire_protocol, model: gateway.model, base_url: gateway.base_url, api_key_set: true,
            request_options: gateway.request_options, runtime_max_output_tokens: gateway.runtime_max_output_tokens,
            configuration_revision: 8, connection_revision: 5, profile_revision: 6 };
        fireEvent.click(screen.getByRole('button', { name: 'Test approval model' }));
        await waitFor(() => expect(h.test).toHaveBeenCalledOnce());
        await expectToast('Passed 2 approval checks');
        expect(h.test.mock.calls[0][0]).toMatchObject({ model: 'main-model', base_url: gateway.base_url,
            request_options: gateway.request_options, runtime_max_output_tokens: 8192 });
        expect(h.test.mock.calls[0][0]).not.toHaveProperty('probe_max_output_tokens');
        expect(screen.getByRole('switch', { name: 'Enable approval model' })).not.toBeChecked();
        expect(h.update).not.toHaveBeenCalled();
    });

    it.each(['disabled', 'untested', 'stale', 'identical', 'partial'])('asks before overwriting %s saved approval settings', async state => {
        if (state === 'disabled') config.enabled = false;
        if (state === 'stale') config.unavailable_reason = 'approval_model_probe_stale';
        if (state === 'identical') config = { ...config, model: gateway.model, request_options: gateway.request_options };
        if (state === 'partial') config = { ...config, wire_protocol: null, model: null, base_url: null, api_key_set: true };
        render(<ApprovalModelSettings />);
        await requestReuse();
        expect(await screen.findByRole('alertdialog')).toHaveTextContent('Overwrite the approval AI configuration?');
        expect(h.reuse).not.toHaveBeenCalled();
        expect(screen.getByRole('button', { name: 'Cancel', exact: true })).toHaveFocus();
    });

    it('cancels without changing the edited model, key, clear-key switch or enablement', async () => {
        await open();
        changeModel();
        fireEvent.change(screen.getByLabelText('API Key'), { target: { value: 'synthetic-draft-key' } });
        fireEvent.click(screen.getByRole('switch', { name: 'Clear stored key' }));
        fireEvent.click(screen.getByRole('switch', { name: 'Enable approval model' }));
        await requestReuse();
        fireEvent.click(screen.getByRole('button', { name: 'Cancel', exact: true }));
        await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull());
        expect(h.reuse).not.toHaveBeenCalled();
        expect(screen.getByLabelText('Model')).toHaveValue('another-review-model');
        expect(screen.getByLabelText('API Key')).toHaveValue('synthetic-draft-key');
        expect(screen.getByRole('switch', { name: 'Clear stored key' })).toBeChecked();
        expect(screen.getByRole('switch', { name: 'Enable approval model' })).not.toBeChecked();
    });

    it('saves after confirmation, replaces draft inputs, and uses the returned revisions for later edits', async () => {
        await open();
        changeModel();
        fireEvent.change(screen.getByLabelText('API Key'), { target: { value: 'synthetic-draft-key' } });
        fireEvent.click(screen.getByRole('switch', { name: 'Enable approval model' }));
        await requestReuse();
        fireEvent.click(screen.getByRole('button', { name: 'Confirm overwrite' }));
        await screen.findByDisplayValue('main-model');
        expect(h.reuse).toHaveBeenCalledOnce();
        expect(screen.getByLabelText('API Key')).toHaveValue('');
        expect(screen.getByRole('switch', { name: 'Enable approval model' })).toBeChecked();
        await expectToast('copied and saved');
        expect(h.test).not.toHaveBeenCalled();
        changeModel();
        await save();
        expect(h.update.mock.calls[0][0]).toMatchObject({ expected_configuration_revision: 8,
            expected_connection_revision: 5, expected_profile_revision: 6 });
    });

    it.each(['incomplete', 'unreadable'])('keeps manual configuration usable when the gateway is %s', async state => {
        if (state === 'incomplete') gateway.api_key_set = false;
        else h.gateway.mockRejectedValueOnce(new Error('offline'));
        await open();
        await screen.findByText(state === 'incomplete'
            ? 'Save a complete model, connection and key in the AI gateway before reusing its configuration.'
            : 'Could not load the AI gateway configuration. Reload saved settings to retry, or continue configuring the approval model manually.');
        expect(reuseButton()).toBeDisabled();
        changeModel();
        await save();
        expect(h.reuse).not.toHaveBeenCalled();
        if (state === 'unreadable') {
            fireEvent.click(screen.getByRole('button', { name: 'Reload saved settings' }));
            await waitFor(() => expect(reuseButton()).toBeEnabled());
        }
    });

    it('preserves input and permits retry after a copy conflict', async () => {
        h.reuse.mockRejectedValue(new Error('approval model configuration revision conflict'));
        await open();
        changeModel();
        await requestReuse();
        fireEvent.click(screen.getByRole('button', { name: 'Confirm overwrite' }));
        await expectToast('revision conflict');
        expect(screen.getByLabelText('Model')).toHaveValue('another-review-model');
        expect(reuseButton()).toBeEnabled();
        expect(h.update).not.toHaveBeenCalled();
        expect(h.test).not.toHaveBeenCalled();
    });

    it('disables duplicate actions throughout a pending copy', async () => {
        let complete!: (value: unknown) => void;
        h.reuse.mockImplementation(() => new Promise(resolve => { complete = resolve; }));
        await open();
        await requestReuse();
        fireEvent.click(screen.getByRole('button', { name: 'Confirm overwrite' }));
        await waitFor(() => expect(reuseButton()).toBeDisabled());
        expect(screen.getByRole('button', { name: 'Save', exact: true })).toBeDisabled();
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeDisabled();
        fireEvent.click(reuseButton());
        expect(h.reuse).toHaveBeenCalledOnce();
        complete({ success: true, data: config });
        await waitFor(() => expect(reuseButton()).toBeEnabled());
    });
});

async function open() {
    render(<ApprovalModelSettings />);
    await screen.findByDisplayValue('review-model');
}
function changeModel() { fireEvent.change(screen.getByLabelText('Model'), { target: { value: 'another-review-model' } }); }
async function save() {
    fireEvent.click(screen.getByRole('button', { name: 'Save', exact: true }));
    await waitFor(() => expect(h.update).toHaveBeenCalledOnce());
}

describe('independent OSS approval model configuration', () => {
    it('configures an initially disabled, empty provider before allowing a probe', async () => {
        config = { ...config, enabled: false, model: null, base_url: null, wire_protocol: null,
            api_key_set: false, unavailable_reason: 'approval_model_disabled' };
        render(<ApprovalModelSettings />);
        await screen.findByRole('switch', { name: 'Enable approval model' });
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeDisabled();
        fireEvent.click(screen.getByRole('switch', { name: 'Enable approval model' }));
        fireEvent.change(screen.getByLabelText('Model'), { target: { value: 'new-review-model' } });
        fireEvent.change(screen.getByLabelText('Base URL'), { target: { value: 'https://example.test/v1' } });
        fireEvent.change(screen.getByLabelText('API Key'), { target: { value: 'test-only-review-key' } });
        await save();
        expect(h.update.mock.calls[0][0]).toMatchObject({ enabled: true, wire_protocol: 'open_ai_chat_completions',
            model: 'new-review-model', api_key: 'test-only-review-key' });
        await waitFor(() => expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled());
    });

    it('persists deactivation without changing any conversation delegation', async () => {
        await open();
        fireEvent.click(screen.getByRole('switch', { name: 'Enable approval model' }));
        await save();
        expect(h.update.mock.calls[0][0].enabled).toBe(false);
        await waitFor(() => expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled());
        expect(h.test).not.toHaveBeenCalled();
    });

    it('tests a complete saved disabled model without enabling it', async () => {
        config = { ...config, enabled: false, unavailable_reason: 'approval_model_disabled' };
        await open();
        const test = screen.getByRole('button', { name: 'Test approval model' });
        expect(test).toBeEnabled();
        fireEvent.click(test);
        await expectToast('Passed 2 approval checks');
        expect(h.test).toHaveBeenCalledOnce();
        expect(h.update).not.toHaveBeenCalled();
        expect(screen.getByRole('switch', { name: 'Enable approval model' })).not.toBeChecked();
        expect(screen.queryByText('Ready. Automatic approval can be enabled separately in each conversation.')).toBeNull();
    });

    it('saves one runtime budget before testing and never submits an independent probe budget', async () => {
        await open();
        expect(screen.queryByLabelText('Probe output tokens')).toBeNull();
        fireEvent.change(screen.getByLabelText('Runtime output tokens'), { target: { value: '8192' } });
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeDisabled();
        await save();
        await waitFor(() => expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled());
        expect(h.update.mock.calls[0][0]).toMatchObject({ runtime_max_output_tokens: 8192 });
        expect(h.update.mock.calls[0][0]).not.toHaveProperty('probe_max_output_tokens');
        fireEvent.click(screen.getByRole('button', { name: 'Test approval model' }));
        await waitFor(() => expect(h.test).toHaveBeenCalledOnce());
        expect(h.test.mock.calls[0][0]).toMatchObject({ runtime_max_output_tokens: 8192 });
        expect(h.test.mock.calls[0][0]).not.toHaveProperty('probe_max_output_tokens');
    });

    it.each([
        { wire_protocol: null }, { model: ' ' }, { base_url: ' ' }, { api_key_set: false },
        { request_options: [] }, { output_limit_field: 'unknown' },
        { runtime_max_output_tokens: 0 }, { max_context_bytes: null },
    ])('keeps testing disabled for an incomplete saved profile %j', async partial => {
        config = { ...config, enabled: false, ...partial };
        render(<ApprovalModelSettings />);
        await screen.findByRole('switch', { name: 'Enable approval model' });
        const test = screen.getByRole('button', { name: 'Test approval model' });
        expect(test).toBeDisabled();
        expect(test).toHaveAccessibleDescription('Save a complete protocol, model, URL, key and request profile, or reuse the AI gateway configuration, before testing the approval model.');
        fireEvent.click(test);
        expect(h.test).not.toHaveBeenCalled();
    });

    it('requires saving a changed enablement state before testing', async () => {
        config = { ...config, enabled: false };
        await open();
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled();
        fireEvent.click(screen.getByRole('switch', { name: 'Enable approval model' }));
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeDisabled();
        expect(screen.getByRole('button', { name: 'Test approval model' })).toHaveAccessibleDescription(
            'You have unsaved changes, including any change to the enable switch. Select Save before testing the approval model.');
        await save();
        await waitFor(() => expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled());
    });

    it('directs the owner to conversation approval after enabling an untested model', async () => {
        config = { ...config, enabled: false, unavailable_reason: 'approval_model_disabled' };
        h.update.mockImplementation(async (payload: ApprovalModelUpdate) => ({ success: true, data: {
            ...config, enabled: payload.enabled, configuration_revision: config.configuration_revision + 1,
            available: true, unavailable_reason: null,
        } }));
        await open();
        fireEvent.click(screen.getByRole('switch', { name: 'Enable approval model' }));
        await save();
        await screen.findByText('Ready. Automatic approval can be enabled separately in each conversation.');
        await expectToast('Open More → AI approval in an AI Assistant conversation');
        expect(screen.getByRole('switch', { name: 'Enable approval model' })).toBeChecked();
        expect(h.test).not.toHaveBeenCalled();
        expect(h.update).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ enabled: true,
            expected_configuration_revision: 7, expected_connection_revision: 3, expected_profile_revision: 4 }));
    });

    it('hydrates masked settings and saves exact revisions without a blank credential', async () => {
        await open();
        expect(screen.getByLabelText('API Key')).toHaveValue('');
        expect(screen.queryByLabelText('Input price per million tokens')).toBeNull();
        changeModel();
        await save();
        const payload = h.update.mock.calls[0][0];
        expect(payload).toMatchObject({
            expected_configuration_revision: 7, expected_connection_revision: 3, expected_profile_revision: 4,
            enabled: true, model: 'another-review-model', request_options: {},
            });
        expect(payload).not.toHaveProperty('api_key');
        expect(payload).not.toHaveProperty('prices');
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled();
        // A second save uses the revisions returned by the first save.
        fireEvent.change(screen.getByLabelText('Model'), { target: { value: 'third-review-model' } });
        fireEvent.click(screen.getByRole('button', { name: 'Save', exact: true }));
        await waitFor(() => expect(h.update).toHaveBeenCalledTimes(2));
        expect(h.update.mock.calls[1][0]).toMatchObject({ expected_configuration_revision: 8,
            expected_connection_revision: 5, expected_profile_revision: 6 });
    });

    it('writes a replacement credential and clears the transient secret after saving', async () => {
        await open();
        fireEvent.change(screen.getByLabelText('API Key'), { target: { value: 'test-only-review-key' } });
        await save();
        expect(h.update.mock.calls[0][0].api_key).toBe('test-only-review-key');
        await waitFor(() => expect(screen.getByLabelText('API Key')).toHaveValue(''));
    });

    it('uses an empty credential only for explicit clearing and disables testing after the clear', async () => {
        await open();
        fireEvent.click(screen.getByRole('switch', { name: 'Clear stored key' }));
        await save();
        expect(h.update.mock.calls[0][0].api_key).toBe('');
        await waitFor(() => expect(screen.getByRole('button', { name: 'Test approval model' })).toBeDisabled());
    });

    it('requires saving before testing and never sends configuration revisions or pricing to the probe', async () => {
        await open();
        changeModel();
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeDisabled();
        await save();
        fireEvent.click(screen.getByRole('button', { name: 'Test approval model' }));
        await waitFor(() => expect(h.test).toHaveBeenCalledOnce());
        expect(h.test.mock.calls[0][0]).toEqual({
            wire_protocol: 'open_ai_chat_completions', model: 'another-review-model', base_url: 'https://example.test/v1',
            reasoning_contract: 'conservative', anthropic_prefix_binding: false,
            request_options: {}, output_limit_field: 'max_tokens',
            runtime_max_output_tokens: 4096, max_context_bytes: 131072,
        });
    });

    it('refreshes authoritative availability after a successful current probe', async () => {
        await open();
        config = { ...config, available: true, unavailable_reason: null };
        fireEvent.click(screen.getByRole('button', { name: 'Test approval model' }));
        await screen.findByText('Ready. Automatic approval can be enabled separately in each conversation.');
        await expectToast('Passed 2 approval checks in 120 ms.');
        expect(h.get).toHaveBeenCalledTimes(2);
    });

    it('does not present an unsaved probe as current validation', async () => {
        h.test.mockResolvedValue({ success: true, data: { saved_as_current: false } });
        await open();
        fireEvent.click(screen.getByRole('button', { name: 'Test approval model' }));
        await expectToast('The configuration changed during testing');
        expect(screen.queryByRole('status')).toBeNull();
    });

    it.each(['null', '[]', '{broken'])('rejects invalid request options %s before saving', async value => {
        await open();
        fireEvent.change(screen.getByLabelText('Advanced request options (JSON)'), { target: { value } });
        fireEvent.click(screen.getByRole('button', { name: 'Save', exact: true }));
        await expectToast('Check the model');
        expect(h.update).not.toHaveBeenCalled();
    });

    it('preserves edits on a revision conflict and reloads only at the owner’s request', async () => {
        h.update.mockRejectedValue(new Error('approval model configuration revision conflict'));
        await open();
        changeModel();
        await save();
        await expectToast('revision conflict');
        expect(screen.getByLabelText('Model')).toHaveValue('another-review-model');
        expect(h.get).toHaveBeenCalledOnce();
        fireEvent.click(screen.getByRole('button', { name: 'Reload saved settings' }));
        await waitFor(() => expect(screen.getByLabelText('Model')).toHaveValue('review-model'));
    });

    it('shows provider probe failures and permits retrying', async () => {
        h.test.mockRejectedValue(new Error('approval model returned the wrong probe verdict'));
        config = { ...config, available: true, unavailable_reason: null };
        await open();
        fireEvent.click(screen.getByRole('button', { name: 'Test approval model' }));
        await expectToast('wrong probe verdict');
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled();
        expect(screen.getByRole('switch', { name: 'Enable approval model' })).toBeChecked();
        expect(screen.getByText('Ready. Automatic approval can be enabled separately in each conversation.')).toBeInTheDocument();
        expect(screen.getByText(/Testing is optional. Missing or failed tests do not block automatic approval/)).toBeInTheDocument();
        expect(h.update).not.toHaveBeenCalled();
    });

    it('offers reload after the initial configuration request fails', async () => {
        h.get.mockRejectedValueOnce(new Error('offline'));
        render(<ApprovalModelSettings />);
        await expectToast('offline');
        expect(screen.queryByLabelText('Model')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'Reload saved settings' }));
        expect(await screen.findByDisplayValue('review-model')).toBeInTheDocument();
    });

    it('shows localized truncation guidance in a toast with the saved runtime limit', async () => {
        h.test.mockRejectedValue(new RestResponseError('approval model probe output was truncated',
            deskErrorCodeEnum.AI_APPROVAL_PROBE_OUTPUT_TRUNCATED, null));
        await open();
        fireEvent.click(screen.getByRole('button', { name: 'Test approval model' }));
        await expectToast('runtime output limit of 4096 tokens');
        expect(h.toast).toHaveBeenLastCalledWith(expect.objectContaining({ variant: 'destructive',
            title: 'Approval model test failed.', duration: 15000,
            description: expect.stringContaining('save and test again'),
        }));
        expect(screen.queryByRole('alert')).toBeNull();
        expect(h.test).toHaveBeenCalledOnce();
        expect(h.update).not.toHaveBeenCalled();
        expect(screen.queryByLabelText('Probe output tokens')).toBeNull();
        expect(screen.getByLabelText('Runtime output tokens')).toHaveValue(4096);
        expect(screen.getByRole('button', { name: 'Test approval model' })).toBeEnabled();
    });

    it('removes the transport wrapper while preserving other model failure details in the toast', async () => {
        h.test.mockRejectedValue(new RestResponseError('Custom desk error(1): approval model returned the wrong probe verdict',
            deskErrorCodeEnum.SYSTEM_ERROR, null));
        await open();
        fireEvent.click(screen.getByRole('button', { name: 'Test approval model' }));
        await expectToast('approval model returned the wrong probe verdict');
        expect(h.toast.mock.calls.at(-1)?.[0].description).not.toContain('Custom desk error');
        expect(screen.queryByRole('alert')).toBeNull();
    });
});
