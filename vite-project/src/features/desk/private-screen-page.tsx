import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Lock } from 'lucide-react';

import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card"
import { ModeToggle } from "@/components/mode-toggle"
import { LanguageToggle } from "@/components/language-toggle"

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
        <div className="flex h-screen w-full items-center justify-center bg-[url('https://mdn.alipayobjects.com/yuyan_qk0oxh/afts/img/V-_oS6r-i7wAAAAAAAAAAAAAFl94AQBr')] bg-cover bg-center select-none cursor-none">
            <div className="absolute top-4 right-4 flex items-center gap-2">
                <LanguageToggle />
                <ModeToggle />
            </div>

            <Card className="w-[380px] sm:w-[460px] shadow-lg bg-white/5 backdrop-blur-md dark:bg-slate-950/40 border-slate-200/20 text-center">
                <CardHeader className="space-y-4 text-center pb-2">
                    <div className="flex justify-center mb-2">
                        <Lock className="h-16 w-16 text-slate-100/90 drop-shadow-lg" />
                    </div>
                    <CardTitle className="text-2xl font-bold tracking-wide text-white drop-shadow-sm">
                        {t('client.privateScreen.title')}
                    </CardTitle>
                    <CardDescription className="text-slate-100/70 text-base leading-relaxed whitespace-pre-line px-4">
                        {t('client.privateScreen.description')}
                    </CardDescription>
                </CardHeader>
                <CardContent className="pt-6 pb-8">
                    <div className="inline-block mt-2 px-6 py-2.5 bg-slate-900/30 border border-slate-100/20 rounded-lg font-mono text-lg text-white tracking-widest backdrop-blur-sm shadow-inner">
                        Ctrl + Alt + L
                    </div>
                    <p className="mt-4 text-sm text-slate-100/50">
                        {t('client.privateScreen.hotkeyHint')}
                    </p>
                </CardContent>
            </Card>
        </div>
    );
}
