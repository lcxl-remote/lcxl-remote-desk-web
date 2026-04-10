import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { useQueryServerInfo } from "@/services/hooks/undefinedController/useQueryServerInfo";
import { Card, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Settings, FileText, Server, Key, Shield, Network } from "lucide-react";

export function SettingsOverview() {
    const { t } = useTranslation();
    const { data: serverInfoResp } = useQueryServerInfo();
    const serverInfo = serverInfoResp?.data;

    if (!serverInfo) {
        return null;
    }

    const isDeskServer = serverInfo.startup_mode === "desk_server" || serverInfo.startup_mode === "desk-server";
    const isSignaling = serverInfo.startup_mode === "signaling";

    return (
        <div className="flex flex-col gap-8 p-6 max-w-6xl mx-auto w-full">
            <div>
                <h2 className="text-2xl font-bold tracking-tight mb-4">
                    {t('pages.settings.category.general', '通用设置')}
                </h2>
                <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                    <Link to="/system/settings" className="block outline-none">
                        <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                            <CardHeader>
                                <div className="flex items-center gap-2">
                                    <Settings className="h-5 w-5 text-primary" />
                                    <CardTitle className="text-lg">{t('menu.settings.system', '系统设置')}</CardTitle>
                                </div>
                                <CardDescription className="mt-2 line-clamp-2">
                                    {t('pages.system.settings.description', '管理全局设备配置和服务器设置')}
                                </CardDescription>
                            </CardHeader>
                        </Card>
                    </Link>
                    <Link to="/system/log" className="block outline-none">
                        <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                            <CardHeader>
                                <div className="flex items-center gap-2">
                                    <FileText className="h-5 w-5 text-primary" />
                                    <CardTitle className="text-lg">{t('menu.settings.log', '日志设置')}</CardTitle>
                                </div>
                                <CardDescription className="mt-2 line-clamp-2">
                                    {t('pages.log.settings.description', '管理应用程序的日志记录级别、格式和自动清理规则。')}
                                </CardDescription>
                            </CardHeader>
                        </Card>
                    </Link>
                </div>
            </div>

            {!isDeskServer && (
                <div>
                    <h2 className="text-2xl font-bold tracking-tight mb-4">
                        {t('pages.settings.category.signal', 'Signal 服务端设置')}
                    </h2>
                    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                        <Link to="/system/turn" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Server className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.turn', 'TURN 设置')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.turn.settings.description', '管理 TURN/STUN 服务器配置')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        <Link to="/system/device-codes" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Key className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.deviceCode', '设备码管理')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.deviceCodeList.description', '管理允许控制服务器的临时设备连接码')}
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
                        {t('pages.settings.category.desk', 'Desk 服务端设置')}
                    </h2>
                    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                        <Link to="/system/turn-client" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Network className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.turnClient', 'TURN 客户端设置')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.turnClient.settings.description', '管理此服务器节点的 TURN/STUN 穿透模式。')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                        <Link to="/system/security" className="block outline-none">
                            <Card className="hover:bg-muted/50 transition-colors h-full cursor-pointer">
                                <CardHeader>
                                    <div className="flex items-center gap-2">
                                        <Shield className="h-5 w-5 text-primary" />
                                        <CardTitle className="text-lg">{t('menu.settings.security', '安全设置')}</CardTitle>
                                    </div>
                                    <CardDescription className="mt-2 line-clamp-2">
                                        {t('pages.system.security.description', '管理远控权限以及授权行为')}
                                    </CardDescription>
                                </CardHeader>
                            </Card>
                        </Link>
                    </div>
                </div>
            )}
        </div>
    );
}
