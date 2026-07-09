import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { Card, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Activity, Bot, Archive } from "lucide-react";

export function UsageOverview() {
    const { t } = useTranslation();

    return (
        <div className="flex flex-col gap-8 p-6 max-w-6xl mx-auto w-full">
            <div>
                <h2 className="text-2xl font-bold tracking-tight mb-4">
                    {t('pages.usage.overview.title')}
                </h2>
                <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                    <Link to="/usage/turn" className="block outline-none">
                        <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                            <CardHeader>
                                <div className="flex items-center gap-2">
                                    <Activity className="h-5 w-5 text-primary" />
                                    <CardTitle className="text-lg">{t('menu.usage.turn')}</CardTitle>
                                </div>
                                <CardDescription className="mt-2 line-clamp-2">
                                    {t('pages.turnUsage.description')}
                                </CardDescription>
                            </CardHeader>
                        </Card>
                    </Link>
                    <Link to="/usage/model" className="block outline-none">
                        <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                            <CardHeader>
                                <div className="flex items-center gap-2">
                                    <Bot className="h-5 w-5 text-primary" />
                                    <CardTitle className="text-lg">{t('menu.usage.model')}</CardTitle>
                                </div>
                                <CardDescription className="mt-2 line-clamp-2">
                                    {t('pages.modelUsage.description')}
                                </CardDescription>
                            </CardHeader>
                        </Card>
                    </Link>
                    <Link to="/usage/retention" className="block outline-none">
                        <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                            <CardHeader>
                                <div className="flex items-center gap-2">
                                    <Archive className="h-5 w-5 text-primary" />
                                    <CardTitle className="text-lg">{t('menu.usage.retention')}</CardTitle>
                                </div>
                                <CardDescription className="mt-2 line-clamp-2">
                                    {t('pages.usageRetention.description')}
                                </CardDescription>
                            </CardHeader>
                        </Card>
                    </Link>
                </div>
            </div>
        </div>
    );
}
