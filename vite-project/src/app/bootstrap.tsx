import type { ReactNode } from 'react';
import type { Root } from 'react-dom/client';

type BootstrapErrorProps = {
    onReload?: () => void;
};

type BootstrapApplicationOptions = {
    application: ReactNode;
    initialize: () => Promise<void>;
    root: Pick<Root, 'render'>;
};

export function BootstrapError({
    onReload = () => window.location.reload(),
}: BootstrapErrorProps) {
    // This fallback must remain independent of locale chunks because it renders
    // when the localization bootstrap itself cannot complete.
    return (
        <main
            className="flex min-h-screen items-center justify-center bg-background p-6 text-foreground"
            role="alert"
        >
            <div className="w-full max-w-lg rounded-lg border bg-card p-6 shadow-sm">
                <h1 className="text-xl font-semibold">
                    应用加载失败 / Application failed to load
                </h1>
                <p className="mt-3 text-sm text-muted-foreground">
                    必要资源加载失败。请检查网络连接，然后重新加载页面。
                </p>
                <p className="mt-1 text-sm text-muted-foreground">
                    A required resource could not be loaded. Check your
                    connection, then reload the page.
                </p>
                <button
                    className="mt-5 rounded-md border px-4 py-2 text-sm font-medium hover:bg-accent"
                    onClick={onReload}
                    type="button"
                >
                    重新加载 / Reload
                </button>
            </div>
        </main>
    );
}

export async function bootstrapApplication({
    application,
    initialize,
    root,
}: BootstrapApplicationOptions): Promise<void> {
    try {
        await initialize();
        root.render(application);
    } catch (error) {
        console.error('Application bootstrap failed', error);
        root.render(<BootstrapError />);
    }
}
