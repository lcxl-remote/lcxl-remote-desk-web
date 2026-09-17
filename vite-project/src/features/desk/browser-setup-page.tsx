import { Link, useLocation, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { ArrowLeft, Monitor } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export default function BrowserSetupPage() {
    const { id } = useParams();
    const { t } = useTranslation();
    const location = useLocation();
    const devicePath = `/desk/${encodeURIComponent(id ?? '')}`;
    const origin = location.state?.returnTo;
    const assistantPath = typeof origin === 'string'
        && [devicePath + '/assistant', devicePath + '/control'].includes(origin.split('?')[0])
        ? origin : devicePath + '/assistant';
    return (
        <div className="h-full overflow-y-auto p-4 sm:p-6">
            <main className="mx-auto max-w-3xl space-y-6 pb-8">
                <Button asChild variant="ghost"><Link to={assistantPath}>
                    <ArrowLeft className="mr-2 h-4 w-4" />{t('pages.deviceAssistant.browserSetup.back')}
                </Link></Button>
                <header className="space-y-2">
                    <h1 className="text-2xl font-semibold">{t('pages.deviceAssistant.browserSetup.title')}</h1>
                    <p className="text-muted-foreground">{t('pages.deviceAssistant.browserSetup.intro')}</p>
                </header>
                <div className="space-y-3 rounded-lg border bg-muted/40 p-4">
                    <p className="text-sm">{t('pages.deviceAssistant.browserSetup.device')}</p>
                    <p className="text-sm text-muted-foreground">{t('pages.deviceAssistant.browserTakeoverBusy')}</p>
                    <Button asChild variant="outline"><Link to={`/desk/${encodeURIComponent(id ?? '')}/control`}>
                        <Monitor className="mr-2 h-4 w-4" />{t('pages.deviceAssistant.browserTakeoverAction')}
                    </Link></Button>
                </div>
                <ol className="space-y-4">
                    {(['install', 'pair', 'permission', 'finish'] as const).map((step, index) => (
                        <li key={step}>
                            <Card>
                                <CardHeader><CardTitle className="text-base">
                                    {index + 1}. {t(`pages.deviceAssistant.browserSetup.${step}Title`)}
                                </CardTitle></CardHeader>
                                <CardContent className="space-y-3 text-sm leading-relaxed">
                                    <p>{t(`pages.deviceAssistant.browserSetup.${step}Body`)}</p>
                                    <p className="text-muted-foreground">{t(`pages.deviceAssistant.browserSetup.${step}Check`)}</p>
                                </CardContent>
                            </Card>
                        </li>
                    ))}
                </ol>
                <section className="space-y-2 rounded-lg border p-4">
                    <h2 className="font-semibold">{t('pages.deviceAssistant.browserSetup.troubleTitle')}</h2>
                    <p className="text-sm leading-relaxed text-muted-foreground">{t('pages.deviceAssistant.browserSetup.troubleBody')}</p>
                </section>
                <Button asChild><Link to={assistantPath}>{t('pages.deviceAssistant.browserSetup.back')}</Link></Button>
            </main>
        </div>
    );
}
