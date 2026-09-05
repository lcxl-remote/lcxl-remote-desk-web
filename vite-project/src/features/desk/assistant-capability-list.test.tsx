import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantCapabilityList } from './assistant-capability-list';
import type { CapabilityInventoryEntry } from './use-device-assistant-capabilities';
import zh from '@/locales/zh-CN/pages';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string, options?: { defaultValue?: string }) =>
    key.startsWith('assistant.capability') ? (zh as Record<string, string>)[key] ?? options?.defaultValue ?? key : options?.defaultValue ?? key }) }));

const entry = (id: string, ready: boolean, reason: CapabilityInventoryEntry['reason']): CapabilityInventoryEntry => ({
    provider_id: 'browser.control', provider_display_name_key: 'provider.browser', provider_version: 1,
    capability: { capability_id: id, tool_name: `tool_${id}`, display_name_key: `capability.${id}`,
        effect: 'read_device', execution_locality: 'edge', execution_policy: 'inline_only',
        limits: { max_input_bytes: 1, max_output_bytes: 2, max_objects: 3, hard_timeout_ms: 4 } },
    context_selectable: true, compiled: true, enabled: true, connected: true, ready, reason,
});

describe('complete device capability inventory', () => {
    it('shows the original key alongside localized name and searchable description', () => {
        const command = entry('system.command.execute', true, null);
        command.capability.display_name_key = 'assistant.capability.systemCommandExecute';
        render(<AssistantCapabilityList entries={[command]} loading={false} error={false} refreshDisabled={false} onRefresh={() => {}} />);
        expect(screen.getByText('执行命令')).toBeTruthy();
        expect(screen.getByText('system.command.execute').tagName).toBe('CODE');
        expect(screen.getByText(zh['assistant.capabilityDescription.systemCommandExecute'])).toBeTruthy();
        fireEvent.change(screen.getByRole('textbox'), { target: { value: '修改文件' } });
        expect(screen.getByText('system.command.execute')).toBeTruthy();
    });

    it('includes unavailable entries, reasons and ready entries without selecting or executing anything', () => {
        const refresh = vi.fn();
        render(<AssistantCapabilityList entries={[entry('ready', true, null), entry('permission', false, 'permission_missing'), entry('unsupported', false, 'unsupported_platform')]}
            loading={false} error={false} refreshDisabled={false} onRefresh={refresh} />);
        expect(screen.getByText('permission_missing')).toBeTruthy();
        expect(screen.getByText('unsupported_platform')).toBeTruthy();
        expect(screen.getAllByText('ready', { exact: true }).length).toBeGreaterThan(0);
        expect(screen.queryAllByRole('checkbox')).toHaveLength(0);
        expect(refresh).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.refreshCapabilities' }));
        expect(refresh).toHaveBeenCalledOnce();
    });

    it('searches unavailable reasons without changing the underlying inventory', () => {
        const entries = [entry('one', true, null), entry('two', false, 'permission_missing')];
        render(<AssistantCapabilityList entries={entries} loading={false} error={false} refreshDisabled={false} onRefresh={() => {}} />);
        fireEvent.change(screen.getByRole('textbox'), { target: { value: 'permission_missing' } });
        expect(screen.getAllByText('two', { exact: true }).length).toBeGreaterThan(0);
        expect(screen.queryAllByText('one', { exact: true })).toHaveLength(0);
        fireEvent.change(screen.getByRole('textbox'), { target: { value: '' } });
        expect(screen.getAllByText('one', { exact: true }).length).toBeGreaterThan(0);
        expect(entries).toHaveLength(2);
    });

    it('keeps errors distinct from an empty inventory and disables offline refresh', () => {
        render(<AssistantCapabilityList entries={[]} loading={false} error refreshDisabled onRefresh={() => {}} />);
        expect(screen.getByRole('alert').textContent).toContain('capabilityLoadError');
        expect(screen.queryByText('pages.deviceAssistant.capabilityEmpty')).toBeNull();
        expect((screen.getByRole('button') as HTMLButtonElement).disabled).toBe(true);
    });
});
