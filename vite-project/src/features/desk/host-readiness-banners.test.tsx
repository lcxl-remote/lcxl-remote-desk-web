import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import '@testing-library/jest-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { deskErrorCodeEnum } from '@/services/types';

import {
    HostReadinessBanners,
    isLoopbackHostname,
    shouldShowWaylandLocalOnlyHint,
    waylandPortalReasonKey,
} from './host-readiness-banners';

const harness = vi.hoisted(() => ({
    info: null as any,
    refetch: vi.fn(async () => undefined),
    toast: vi.fn(),
    authorize: vi.fn(async () => undefined),
    cancel: vi.fn(async () => undefined),
    requestMacos: vi.fn(async () => undefined),
    queryOptions: null as any,
}));

vi.mock('react-i18next', () => ({
    useTranslation: () => ({
        t: (key: string, options?: { permissions?: string }) => options?.permissions
            ? `${key}:${options.permissions}`
            : key,
    }),
}));

vi.mock('@/hooks/use-toast', () => ({
    useToast: () => ({ toast: harness.toast }),
}));

vi.mock('@/services/hooks/systemController/useQueryServerInfo', () => ({
    useQueryServerInfo: (options: any) => {
        harness.queryOptions = options;
        return {
        data: harness.info ? { data: harness.info } : undefined,
        refetch: harness.refetch,
        };
    },
}));

vi.mock('@/services/hooks/hostReadinessController/useAuthorizeWayland', () => ({
    useAuthorizeWayland: () => ({ mutateAsync: harness.authorize }),
}));

vi.mock('@/services/hooks/hostReadinessController/useCancelWayland', () => ({
    useCancelWayland: () => ({ mutateAsync: harness.cancel }),
}));

vi.mock('@/services/hooks/hostReadinessController/useRequestMacosPermissions', () => ({
    useRequestMacosPermissions: () => ({ mutateAsync: harness.requestMacos }),
}));

vi.mock('@/features/layout/service-install-dialog', () => ({
    ServiceInstallDialog: () => null,
}));

function baseInfo(overrides: Record<string, unknown> = {}) {
    return {
        platform: 'linux',
        startup_mode: 'default',
        service_installed: false,
        service_running: false,
        server_binary_available: true,
        is_admin: true,
        default_install_path: '',
        macos_permissions: null,
        wayland_portal: null,
        ...overrides,
    };
}

describe('HostReadinessBanners', () => {
    beforeEach(() => {
        harness.info = null;
        harness.refetch.mockClear();
        harness.toast.mockClear();
        harness.authorize.mockClear();
        harness.cancel.mockClear();
        harness.requestMacos.mockClear();
        harness.queryOptions = null;
    });

    it('recognizes only loopback web origins as local permission surfaces', () => {
        expect(isLoopbackHostname('localhost')).toBe(true);
        expect(isLoopbackHostname('127.0.0.1')).toBe(true);
        expect(isLoopbackHostname('[::1]')).toBe(true);
        expect(isLoopbackHostname('192.168.50.6')).toBe(false);
        expect(isLoopbackHostname('desk-host.local')).toBe(false);
    });

    it('hides the local-only hint after screen and input are fully ready', () => {
        expect(shouldShowWaylandLocalOnlyHint(false, 'ready', true, false)).toBe(false);
        expect(shouldShowWaylandLocalOnlyHint(false, 'ready', false, false)).toBe(true);
        expect(shouldShowWaylandLocalOnlyHint(false, 'needs_authorization', false, true)).toBe(true);
        expect(shouldShowWaylandLocalOnlyHint(true, 'needs_authorization', false, true)).toBe(false);
    });

    it('localizes Portal failures by error code without rendering diagnostic detail', () => {
        harness.info = baseInfo({
            wayland_portal: {
                phase: 'failed',
                screen_ready: false,
                input_ready: false,
                target: 'screen_and_input',
                recommended_target: 'screen_and_input',
                operation_id: null,
                generation: 8,
                persistent_restore: false,
                requires_local_action: true,
                reason_code: deskErrorCodeEnum.WAYLAND_PORTAL_INPUT_PERMISSION_REQUIRED,
                reason: 'portal did not grant both keyboard and pointer input',
            },
        });

        render(<HostReadinessBanners />);

        expect(screen.getByText(
            'pages.hostReadiness.wayland.inputPermissionRequired',
        )).toBeInTheDocument();
        expect(screen.queryByText(
            'portal did not grant both keyboard and pointer input',
        )).not.toBeInTheDocument();
        expect(waylandPortalReasonKey(-1)).toBe(
            'pages.hostReadiness.wayland.genericFailure',
        );
    });

    it('upgrades a screen-only session to screen and input', async () => {
        harness.info = baseInfo({
            wayland_portal: {
                phase: 'ready',
                screen_ready: true,
                input_ready: false,
                target: 'screen_only',
                recommended_target: 'screen_only',
                operation_id: null,
                generation: 7,
                persistent_restore: true,
                requires_local_action: false,
                reason: null,
            },
        });

        render(<HostReadinessBanners />);
        expect(screen.getByText(
            'pages.hostReadiness.wayland.enableInputDescription',
        )).toBeInTheDocument();
        fireEvent.click(screen.getByText('pages.hostReadiness.wayland.enableInput'));

        await waitFor(() => expect(harness.authorize).toHaveBeenCalledTimes(1));
        expect(harness.authorize).toHaveBeenCalledWith({
            data: {
                operation_id: expect.any(String),
                target: 'screen_and_input',
            },
        });
        expect(harness.refetch).toHaveBeenCalledTimes(1);
    });

    it('cancels only the currently reported Portal operation generation', async () => {
        harness.info = baseInfo({
            wayland_portal: {
                phase: 'preparing',
                screen_ready: false,
                input_ready: false,
                target: 'screen_and_input',
                recommended_target: 'screen_and_input',
                operation_id: 'op-9',
                generation: 9,
                persistent_restore: true,
                requires_local_action: true,
                reason: null,
            },
        });

        render(<HostReadinessBanners />);
        fireEvent.click(screen.getByText('common.cancel'));

        await waitFor(() => expect(harness.cancel).toHaveBeenCalledTimes(1));
        expect(harness.cancel).toHaveBeenCalledWith({
            data: { operation_id: 'op-9', generation: 9 },
        });
    });

    it('polls quickly only while Portal authorization is pending', () => {
        harness.info = baseInfo();
        render(<HostReadinessBanners />);

        const interval = harness.queryOptions.query.refetchInterval;
        expect(interval({
            state: {
                data: {
                    data: baseInfo({
                        wayland_portal: { phase: 'preparing' },
                    }),
                },
            },
        })).toBe(2_000);
        expect(interval({
            state: {
                data: {
                    data: baseInfo({
                        wayland_portal: { phase: 'ready' },
                    }),
                },
            },
        })).toBe(30_000);
        expect(interval({
            state: { data: { data: baseInfo({ platform: 'windows' }) } },
        })).toBe(30_000);
    });

    it('reports Input Monitoring independently and requests it only locally', async () => {
        harness.info = baseInfo({
            platform: 'macos',
            macos_permissions: {
                screen_recording: true,
                accessibility: true,
                input_monitoring: false,
            },
        });

        render(<HostReadinessBanners />);

        expect(screen.getByText(
            'pages.hostReadiness.macos.description:pages.system.settings.macos.permissions.inputMonitoring',
        )).toBeInTheDocument();
        fireEvent.click(screen.getByText('pages.hostReadiness.macos.action'));

        await waitFor(() => expect(harness.requestMacos).toHaveBeenCalledTimes(1));
        expect(harness.refetch).toHaveBeenCalledTimes(1);
    });
});
