import { createBrowserRouter, Navigate } from 'react-router-dom';
import App from '@/App';
import LoginPage from '@/features/auth/login-page';
import InitPage from '@/features/auth/init-page';
import PrivateScreenPage from '@/features/desk/private-screen-page';
import Layout from '@/features/layout/layout';
import DeskList from '@/features/desk/desk-list';
import DeskDashboard from '@/features/desk/desk-dashboard';
import DeskSession from '@/features/desk/desk-session';
import FileManager from '@/features/file-manager/file-list';
import TerminalSession from '@/features/terminal/terminal-session';
import RequireAuth from '@/features/auth/require-auth';
import { SystemSettings } from '@/features/settings/system-settings';
import { UserSettings } from '@/features/settings/user-settings';
import { DeviceCodeList } from '@/features/settings/device-code-list';

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
                path: 'system/settings',
                element: <SystemSettings />,
            },
            {
                path: 'system/device-codes',
                element: <DeviceCodeList />,
            },
            {
                path: 'user/settings',
                element: <UserSettings />,
            },
        ]
    },
]);
