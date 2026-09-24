import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Bell, CalendarClock } from 'lucide-react';
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';

export function AiAssistantOverview() {
    const { t } = useTranslation();

    return <div className="mx-auto w-full max-w-6xl p-6">
        <h1 className="mb-4 text-2xl font-bold tracking-tight">{t('pages.aiAssistant.title')}</h1>
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
            <Link to="/ai-assistant/attention" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <Bell className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAssistantAttention.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAssistantAttention.description')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/schedules" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <CalendarClock className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('schedules.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAssistant.overview.schedulesDescription')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
        </div>
    </div>;
}
