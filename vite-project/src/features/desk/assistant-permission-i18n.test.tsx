import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { createInstance } from 'i18next';
import { I18nextProvider } from 'react-i18next';
import { describe, expect, it, vi } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';
import type { PermissionRequestDto } from '@/services/types';
import { AssistantPermissionRequest } from './assistant-permission-request';
import { permissionOperationLabel, permissionResourceLabel, permissionToolLabel } from './assistant-permission-labels';

describe('permission internationalization', () => {
    it('switches language without changing selected authorization identifiers', async () => {
        const i18n = createInstance();
        await i18n.init({ lng: 'zh-CN', fallbackLng: 'en-US',
            resources: { 'zh-CN': { translation: zh }, 'en-US': { translation: en } } });
        const request: PermissionRequestDto = {
            schemaVersion: 1, createdAt: '2026-09-19T00:00:00Z',
            requestId: 'translated', inputRevision: 1, state: 'pending', items: [{
                itemId: 'read', providerId: 'desktop.session', toolName: 'inspect_desktop_session',
                reason: 'User-provided reason', expectedEffect: 'read_device',
                resourceScope: ['target:current_device', 'target:another-device'], operationScope: ['observe'],
                exportDestinations: [], suggestedMaxUses: 2, suggestedTtlSeconds: 300,
            }],
        };
        const onDecide = vi.fn().mockResolvedValue(true);
        render(<I18nextProvider i18n={i18n}><AssistantPermissionRequest request={request} canDecide onDecide={onDecide} /></I18nextProvider>);
        expect(screen.getAllByText('查看桌面会话').length).toBeGreaterThan(0);
        expect(screen.getByRole('checkbox', { name: '当前被控设备' })).toBeChecked();
        expect(screen.getByRole('checkbox', { name: '查看信息' })).toBeChecked();
        fireEvent.click(screen.getByRole('checkbox', { name: '设备：another-device' }));
        await act(() => i18n.changeLanguage('en-US'));
        expect(screen.getByRole('checkbox', { name: 'Current target device' })).toBeChecked();
        expect(screen.getByRole('checkbox', { name: 'Device: another-device' })).not.toBeChecked();
        expect(screen.getByRole('checkbox', { name: 'Observe' })).toBeChecked();
        const approveText = i18n.t('pages.aiAssistant.permissionSubmitSelection');
        fireEvent.click(screen.getByRole('button', { name: approveText }));
        await waitFor(() => expect(onDecide).toHaveBeenCalledWith(request, [{
            itemId: 'read', decision: 'approve', resource_scope: ['target:current_device'],
            operation_scope: ['observe'], export_destinations: [], ttl_seconds: 300, max_uses: 2,
        }]));
    });

    it('preserves unknown identifiers and distinct resource identities', async () => {
        const i18n = createInstance();
        await i18n.init({ lng: 'zh-CN', fallbackLng: 'en-US',
            resources: { 'zh-CN': { translation: zh } } });
        expect(permissionToolLabel(i18n.t, 'future_tool')).toBe('future_tool');
        expect(permissionOperationLabel(i18n.t, 'ui:future_action')).toBe('ui:future_action');
        expect(permissionResourceLabel(i18n.t, 'unknown:path:keep')).toBe('unknown:path:keep');
        expect(permissionResourceLabel(i18n.t, 'selected:sha256:abc')).toBe('所选对象：sha256:abc');
        expect(permissionResourceLabel(i18n.t, 'selected:sha256:def')).toBe('所选对象：sha256:def');
        expect(permissionOperationLabel(i18n.t, 'launch_application')).toBe('启动应用');
    });
});
