import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { useQueryServerInfo } from "@/services/hooks/systemController/useQueryServerInfo";
import { Card, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Settings, FileText, Server, Key, Shield, ShieldCheck, Network, Monitor, Bot, KeyRound, Link2 } from "lucide-react";

export function SettingsOverview() {
    const { t } = useTranslation();
    const { data: serverInfoResp } = useQueryServerInfo();
    const serverInfo = serverInfoResp?.data;

    if (!serverInfo) {
        return null;
    }

    const isDeskServer = serverInfo.startup_mode === "desk_server" || serverInfo.startup_mode === "desk-server";
    const isSignaling = serverInfo.startup_mode === "signaling";
    // macOS-only ServerInfo field; non-null only on macOS. The IDD virtual
    // display is Windows-only, so its entry is hidden on macOS.
    const isMac = serverInfo.background_start != null;

    return (
        <div className="flex flex-col gap-8 p-6 max-w-6xl mx-auto w-full">
            <div>
                <h2 className="text-2xl font-bold tracking-tight mb-4">
                    {t('pages.settings.category.general')}
                </h2>
                <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                    <Link to="/system/settings" className="block outline-none">
                        <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                            <CardHeader>
                                <div className="flex items-center gap-2">
                                    <Settings className="h-5 w-5 text-primary" />
                                    <CardTitle className="text-lg">{t('menu.settings.system')}</CardTitle>
                                </div>
                                <CardDescription className="mt-2 line-clamp-2">
                                    {t('pages.system.settings.description')}
                                </CardDescription>
                            </CardHeader>
                        </Card>
                    </Link>
                    <Link to="/system/log" className="block outline-none">
                        <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                            <CardHeader>
                                <div className="flex items-center gap-2">
                                    <FileText className="h-5 w-5 text-primary" />
                                    <CardTitle className="text-lg">{t('menu.settings.log')}</CardTitle>
                                </div>
                                <CardDescription className="mt-2 line-clamp-2">
                                    {t('pages.log.settings.description')}
                                </CardDescription>
                            </CardHeader>
                        </Card>
                    </Link>
                </div>
            </div>

            {!isDeskServer && (
                <div>
                    <h2 className="text-2xl font-bold tracking-tight mb-4">
                        {t('pages.settings.category.signal')}
                    </h2>
                    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                        <Link to="/system/turn" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Server className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.turn')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.turn.settings.description')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        <Link to="/system/signal-token" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <KeyRound className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.signalToken')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.signalToken.description')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        <Link to="/system/device-codes" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Key className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.deviceCode')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.deviceCodeList.description')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        <Link to="/system/ai-model" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Bot className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.aiModel')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.aiModel.settings.description')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                    </div>
                </div>
            )}

            {!isSignaling && (
                <div>
                    <h2 className="text-2xl font-bold tracking-tight mb-4">
                        {t('pages.settings.category.desk')}
                    </h2>
                    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                        <Link to="/system/turn-client" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Network className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.turnClient')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.turnClient.settings.description')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        <Link to="/system/desk-connection" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Link2 className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.deskConnection')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.deskConnection.description')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        <Link to="/system/security" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Shield className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.security')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.system.security.description')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        <Link to="/system/ai-policy" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <ShieldCheck className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.aiPolicy')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.aiPolicy.settings.description')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        {!isMac && (
                            <Link to="/system/virtual-display" className="block outline-none">
                                <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                    <CardHeader>
                                        <div className="flex items-center gap-2">
                                            <Monitor className="h-5 w-5 text-primary" />
                                            <CardTitle className="text-lg">{t('menu.settings.virtualDisplay')}</CardTitle>
                                        </div>
                                        <CardDescription className="mt-2 line-clamp-2">
                                            {t('pages.virtualDisplay.description')}
                                        </CardDescription>
                                    </CardHeader>
                                </Card>
                            </Link>
                        )}
                    </div>
                </div>
            )}
        </div>
    );
}
