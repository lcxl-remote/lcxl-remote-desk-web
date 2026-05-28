import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { RouterProvider } from 'react-router-dom';
import { Toaster } from '@/components/ui/toaster';
import { ThemeProvider } from '@/components/theme-provider';
import { GlobalErrorBoundary } from '@/components/error-boundary';
import { router } from './router';
const queryClient = new QueryClient({
    defaultOptions: {
        queries: {
            retry: false, // Turn off automatic retries on errors for faster UI feedback
        },
        mutations: {
            retry: false,
        },
    },
});
export function AppProviders() {
    return (
        <QueryClientProvider client={queryClient}>
            <ThemeProvider defaultTheme="dark" storageKey="vite-ui-theme">
                <GlobalErrorBoundary>
                    <RouterProvider router={router} />
                </GlobalErrorBoundary>
                <Toaster />
            </ThemeProvider>
        </QueryClientProvider>
    );
}
