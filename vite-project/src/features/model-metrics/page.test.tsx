import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { startupModeEnum, type CurrentUserDto, type StartupMode } from '@/services/types';
import ModelMetricsPage from './page';
import ModelMetricsSettingsPage from '@/features/settings/model-metrics-settings';

const state = vi.hoisted(() => ({ user: undefined as CurrentUserDto | undefined, mode: undefined as StartupMode | undefined }));
vi.mock('@/services/hooks/userController/useGetCurrentUser', () => ({ useGetCurrentUser: () => ({ data: { data: state.user } }) }));
vi.mock('@/services/hooks/systemController/useQueryServerInfo', () => ({ useQueryServerInfo: () => ({ data: { data: { startup_mode: state.mode } } }) }));
vi.mock('./api', () => ({ createMetricsApi: () => ({}) }));
vi.mock('./local-usage', () => ({ LocalModelUsage: () => null }));
vi.mock('./dashboard', () => ({ MetricsDashboard: ({ allowed, identity }: { allowed: boolean; identity: string }) =>
    <div data-testid="metrics-entry" data-allowed={String(allowed)}>{identity}</div> }));
vi.mock('./settings', () => ({ MetricsCollectionSettings: ({ allowed, identity }: { allowed: boolean; identity: string }) =>
    <div data-testid="metrics-entry" data-allowed={String(allowed)}>{identity}</div> }));
afterEach(() => { cleanup(); state.user = undefined; state.mode = undefined; });

describe.each([ModelMetricsPage, ModelMetricsSettingsPage])('OSS model metrics entry %s', (Page) => {
    it('keeps access closed until both owner identity and a supported startup mode are resolved', () => {
        const view = render(<Page/>);
        expect(screen.getByTestId('metrics-entry').getAttribute('data-allowed')).toBe('false');
        state.user = { name: 'owner', access: 'admin' };
        view.rerender(<Page/>);
        expect(screen.getByTestId('metrics-entry').getAttribute('data-allowed')).toBe('false');
        state.mode = startupModeEnum.signaling;
        view.rerender(<Page/>);
        expect(screen.getByTestId('metrics-entry').getAttribute('data-allowed')).toBe('true');
    });

    it('closes the page and changes cache identity when the owner becomes target-scoped', () => {
        state.user = { name: 'owner', user_id: 1, access: 'admin' };
        state.mode = startupModeEnum.default;
        const view = render(<Page/>);
        const original = screen.getByTestId('metrics-entry').textContent;
        state.user = { ...state.user, target_connection_id: 'one-device' };
        view.rerender(<Page/>);
        expect(screen.getByTestId('metrics-entry').getAttribute('data-allowed')).toBe('false');
        expect(screen.getByTestId('metrics-entry').textContent).not.toBe(original);
        state.mode = startupModeEnum['desk-server'];
        state.user = { name: 'owner', access: 'admin' };
        view.rerender(<Page/>);
        expect(screen.getByTestId('metrics-entry').getAttribute('data-allowed')).toBe('false');
    });
});
