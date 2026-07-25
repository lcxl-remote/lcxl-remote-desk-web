import { createBrowserRouter, Navigate } from 'react-router-dom';

export const router = createBrowserRouter([
    {
        path: '/user/login',
        lazy: async () => ({
            Component: (await import('@/features/auth/login-page')).default,
        }),
    },
    {
        path: '/init',
        lazy: async () => ({
            Component: (await import('@/features/auth/init-page')).default,
        }),
    },
    {
        path: '/private-screen',
        lazy: async () => ({
            Component: (await import('@/features/desk/private-screen-page')).default,
        }),
    },
    {
        path: '/whiteboard',
        lazy: async () => ({
            Component: (await import('@/features/desk/whiteboard-page')).default,
        }),
    },
    {
        path: '/security-approval',
        lazy: async () => ({
            Component: (await import('@/features/desk/security-approval-page')).default,
        }),
    },
    {
        path: '/host-access-status',
        lazy: async () => ({
            Component: (await import('@/features/desk/host-access-status-page')).default,
        }),
    },
    {
        path: '/',
        lazy: async () => ({
            Component: (await import('@/features/layout/layout')).AuthenticatedLayout,
        }),
        children: [
            {
                index: true,
                element: <Navigate to="/desk/list" replace />,
            },
            {
                path: 'app',
                lazy: async () => ({
                    Component: (await import('@/App')).default,
                }),
            },
            {
                path: 'desk/list',
                lazy: async () => ({
                    Component: (await import('@/features/desk/desk-list')).default,
                }),
            },
            {
                path: 'desk/:id',
                lazy: async () => ({
                    Component: (await import('@/features/desk/desk-dashboard')).default,
                }),
            },
            {
                path: 'desk/:id/control',
                lazy: async () => ({
                    Component: (await import('@/features/desk/desk-session')).default,
                }),
            },
            {
                path: 'desk/:id/files',
                lazy: async () => ({
                    Component: (await import('@/features/file-manager/file-list')).default,
                }),
            },
            {
                path: 'desk/:id/terminal',
                lazy: async () => ({
                    Component: (await import('@/features/terminal/terminal-session-launcher')).default,
                }),
            },
            {
                path: 'support',
                lazy: async () => ({
                    Component: (await import('@/features/support/support-page')).SupportPage,
                }),
            },
            {
                path: 'system',
                lazy: async () => ({
                    Component: (await import('@/features/settings/settings-layout')).SettingsLayout,
                }),
                children: [
                    {
                        index: true,
                        lazy: async () => ({
                            Component: (await import('@/features/settings/settings-overview')).SettingsOverview,
                        }),
                    },
                    {
                        path: 'settings',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/system-settings')).SystemSettings,
                        }),
                    },
                    {
                        path: 'turn',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/turn-settings')).TurnSettings,
                        }),
                    },
                    {
                        path: 'turn-client',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/turn-client-settings')).TurnClientSettingsPage,
                        }),
                    },
                    {
                        path: 'desk-connection',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/desk-connection-settings')).DeskConnectionSettings,
                        }),
                    },
                    {
                        path: 'signal-token',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/signal-token-settings')).SignalTokenSettings,
                        }),
                    },
                    {
                        path: 'log',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/log-settings')).LogSettings,
                        }),
                    },
                    {
                        path: 'security',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/security-settings')).SecuritySettings,
                        }),
                    },
                    {
                        path: 'device-codes',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/device-code-list')).DeviceCodeList,
                        }),
                    },
                    {
                        path: 'virtual-display',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/virtual-display-settings')).VirtualDisplaySettings,
                        }),
                    },
                    {
                        path: 'ai-model',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/ai-model-settings')).AiModelSettings,
                        }),
                    },
                    {
                        path: 'ai-policy',
                        lazy: async () => ({
                            Component: (await import('@/features/settings/ai-policy-settings')).AiPolicySettings,
                        }),
                    },
                ]
            },
            {
                path: 'usage',
                lazy: async () => ({
                    Component: (await import('@/features/usage/usage-layout')).UsageLayout,
                }),
                children: [
                    {
                        index: true,
                        lazy: async () => ({
                            Component: (await import('@/features/usage/usage-overview')).UsageOverview,
                        }),
                    },
                    {
                        path: 'turn',
                        lazy: async () => ({
                            Component: (await import('@/features/usage/turn-usage')).TurnUsagePage,
                        }),
                    },
                    {
                        path: 'model',
                        lazy: async () => ({
                            Component: (await import('@/features/usage/model-usage')).ModelUsagePage,
                        }),
                    },
                    {
                        path: 'retention',
                        lazy: async () => ({
                            Component: (await import('@/features/usage/usage-retention')).UsageRetentionPage,
                        }),
                    },
                ]
            },
            {
                path: 'user/settings',
                lazy: async () => ({
                    Component: (await import('@/features/settings/user-settings')).UserSettings,
                }),
            },
        ]
    },
]);
