import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Activity, Bell, Brain, CalendarClock, ListTodo, Search, SlidersHorizontal } from 'lucide-react';
import { Card, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { AiAssistantIcon } from '@/components/ai-assistant-icon';

export function AiAssistantOverview({ showWorkbench = true }: { showWorkbench?: boolean }) {
    const { t } = useTranslation();

    return <div className="mx-auto w-full max-w-6xl p-6">
        <h1 className="mb-4 text-2xl font-bold tracking-tight">{t('pages.aiAssistant.title')}</h1>
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
            {showWorkbench && <Link to="/ai-assistant/workbench" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <ListTodo className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAssistant.workbench.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAssistant.workbench.description')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>}
            <Link to="/ai-assistant/conversations" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <AiAssistantIcon className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAssistant.overview.conversationsTitle')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAssistant.overview.conversationsDescription')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
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
            <Link to="/ai-assistant/personalization" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <SlidersHorizontal className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAssistant.personalization.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAssistant.personalization.description')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-assistant/processing" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <Activity className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAssistant.processing.title')}</CardTitle>
                        </div>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-assistant/history" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <Search className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAssistant.historySearch.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAssistant.historySearch.description')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
            <Link to="/ai-assistant/memories" className="block rounded-xl outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
                <Card className="h-full cursor-pointer transition-colors hover:bg-muted/50">
                    <CardHeader>
                        <div className="flex items-center gap-2">
                            <Brain className="h-5 w-5 text-primary" aria-hidden="true" />
                            <CardTitle className="text-lg">{t('pages.aiAssistant.memory.title')}</CardTitle>
                        </div>
                        <CardDescription className="mt-2 line-clamp-2">{t('pages.aiAssistant.memory.overviewDescription')}</CardDescription>
                    </CardHeader>
                </Card>
            </Link>
        </div>
    </div>;
}
