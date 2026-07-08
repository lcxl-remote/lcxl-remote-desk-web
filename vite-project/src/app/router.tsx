import { createBrowserRouter, Navigate } from 'react-router-dom';
import App from '@/App';
import LoginPage from '@/features/auth/login-page';
import InitPage from '@/features/auth/init-page';
import PrivateScreenPage from '@/features/desk/private-screen-page';
import WhiteboardPage from '@/features/desk/whiteboard-page';
import SecurityApprovalPage from '@/features/desk/security-approval-page';
import Layout from '@/features/layout/layout';
import DeskList from '@/features/desk/desk-list';
import DeskDashboard from '@/features/desk/desk-dashboard';
import DeskSession from '@/features/desk/desk-session';
import FileManager from '@/features/file-manager/file-list';
import TerminalSession from '@/features/terminal/terminal-session';
import RequireAuth from '@/features/auth/require-auth';
import { SystemSettings } from '@/features/settings/system-settings';
import { TurnSettings } from '@/features/settings/turn-settings';
import { TurnUsagePage } from '@/features/usage/turn-usage';
import { ModelUsagePage } from '@/features/usage/model-usage';
import { UsageLayout } from '@/features/usage/usage-layout';
import { UsageOverview } from '@/features/usage/usage-overview';
import { TurnClientSettingsPage } from '@/features/settings/turn-client-settings';
import { LogSettings } from '@/features/settings/log-settings';
import { SecuritySettings } from '@/features/settings/security-settings';
import { UserSettings } from '@/features/settings/user-settings';
import { DeviceCodeList } from '@/features/settings/device-code-list';
import { SettingsLayout } from '@/features/settings/settings-layout';
import { SettingsOverview } from '@/features/settings/settings-overview';
import { VirtualDisplaySettings } from '@/features/settings/virtual-display-settings';
import { AiModelSettings } from '@/features/settings/ai-model-settings';
import { DeskConnectionSettings } from '@/features/settings/desk-connection-settings';
import { SignalTokenSettings } from '@/features/settings/signal-token-settings';
import { SupportPage } from '@/features/support/support-page';

export const router = createBrowserRouter([
    {
        path: '/user/login',
        element: <LoginPage />,
    },
    {
        path: '/init',
        element: <InitPage />,
    },
    {
        path: '/private-screen',
        element: <PrivateScreenPage />,
    },
    {
        path: '/whiteboard',
        element: <WhiteboardPage />,
    },
    {
        path: '/security-approval',
        element: <SecurityApprovalPage />,
    },
    {
        path: '/',
        element: (
            <RequireAuth>
                <Layout />
            </RequireAuth>
        ),
        children: [
            {
                index: true,
                element: <Navigate to="/desk/list" replace />,
            },
            {
                path: 'app',
                element: <App />,
            },
            {
                path: 'desk/list',
                element: <DeskList />,
            },
            {
                path: 'desk/:id',
                element: <DeskDashboard />,
            },
            {
                path: 'desk/:id/control',
                element: <DeskSession />,
            },
            {
                path: 'desk/:id/files',
                element: <FileManager />,
            },
            {
                path: 'desk/:id/terminal',
                element: <TerminalSession />,
            },
            {
                path: 'support',
                element: <SupportPage />,
            },
            {
                path: 'system',
                element: <SettingsLayout />,
                children: [
                    {
                        index: true,
                        element: <SettingsOverview />,
                    },
                    {
                        path: 'settings',
                        element: <SystemSettings />,
                    },
                    {
                        path: 'turn',
                        element: <TurnSettings />,
                    },
                    {
                        path: 'turn-client',
                        element: <TurnClientSettingsPage />,
                    },
                    {
                        path: 'desk-connection',
                        element: <DeskConnectionSettings />,
                    },
                    {
                        path: 'signal-token',
                        element: <SignalTokenSettings />,
                    },
                    {
                        path: 'log',
                        element: <LogSettings />,
                    },
                    {
                        path: 'security',
                        element: <SecuritySettings />,
                    },
                    {
                        path: 'device-codes',
                        element: <DeviceCodeList />,
                    },
                    {
                        path: 'virtual-display',
                        element: <VirtualDisplaySettings />,
                    },
                    {
                        path: 'ai-model',
                        element: <AiModelSettings />,
                    },
                ]
            },
            {
                path: 'usage',
                element: <UsageLayout />,
                children: [
                    {
                        index: true,
                        element: <UsageOverview />,
                    },
                    {
                        path: 'turn',
                        element: <TurnUsagePage />,
                    },
                    {
                        path: 'model',
                        element: <ModelUsagePage />,
                    },
                ]
            },
            {
                path: 'user/settings',
                element: <UserSettings />,
            },
        ]
    },
]);
