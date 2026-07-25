import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Lock } from 'lucide-react';

/**
 * The overlay the host machine shows while a remote session holds the privacy
 * screen.
 *
 * It must render opaquely the moment it is loaded, with no network fetch in the
 * way: anything that can fail to load is a chance for the real desktop to stay
 * visible on the physical display. The page is also click-through and never
 * takes keyboard focus, so it carries no interactive controls.
 */
export default function PrivateScreenPage() {
    const { t } = useTranslation();

    useEffect(() => {
        // Enforce privacy styles globally when mounted
        document.body.style.overflow = 'hidden';
        document.body.style.userSelect = 'none';
        document.body.style.cursor = 'none';

        return () => {
            // Restore styles on unmount
            document.body.style.overflow = '';
            document.body.style.userSelect = '';
            document.body.style.cursor = '';
        };
    }, []);

    return (
        <div
            data-testid="private-screen-root"
            className="flex h-screen w-full items-center justify-center bg-slate-950 bg-gradient-to-br from-slate-900 via-slate-950 to-black select-none cursor-none"
        >
            <div className="w-[380px] sm:w-[460px] text-center px-6">
                <div className="flex justify-center mb-6">
                    <Lock className="h-16 w-16 text-slate-100/90 drop-shadow-lg" />
                </div>
                <h1 className="text-2xl font-bold tracking-wide text-white drop-shadow-sm">
                    {t('client.privateScreen.title')}
                </h1>
                <p className="mt-4 text-slate-100/70 text-base leading-relaxed whitespace-pre-line">
                    {t('client.privateScreen.description')}
                </p>
                <div className="inline-block mt-8 px-6 py-2.5 bg-slate-900 border border-slate-100/20 rounded-lg font-mono text-lg text-white tracking-widest shadow-inner">
                    Ctrl + Alt + L
                </div>
                <p className="mt-4 text-sm text-slate-100/50">
                    {t('client.privateScreen.hotkeyHint')}
                </p>
            </div>
        </div>
    );
}
