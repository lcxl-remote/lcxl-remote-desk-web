import { useParams, useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { Monitor, Terminal as TerminalIcon, Folder, ArrowLeft, Globe, Server, Lock, Bot } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle, CardFooter } from "@/components/ui/card"
import { useListConnections } from "@/services/hooks/connectionController/useListConnections"
import { Skeleton } from "@/components/ui/skeleton"
import { Badge } from "@/components/ui/badge"
import { useRestrictedSession } from "@/features/desk/restricted-session"

const OSS_DEVICE_ASSISTANT_ENABLED = import.meta.env.BASE_URL !== '/console/';

export default function DeskDashboard({
    showAssistant = OSS_DEVICE_ASSISTANT_ENABLED,
}: { showAssistant?: boolean }) {
    const { id: deskId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    const { data: connections, isLoading } = useListConnections()
    const restricted = useRestrictedSession(deskId)

    const connection = connections?.find((s: any) => s.connection_id === deskId)

    if (isLoading) {
        return (
            <div className="p-6 space-y-6">
                <Skeleton className="h-8 w-[200px]" />
                <div className="grid gap-4 md:grid-cols-3">
                    <Skeleton className="h-[150px] w-full" />
                    <Skeleton className="h-[150px] w-full" />
                    <Skeleton className="h-[150px] w-full" />
                </div>
            </div>
        )
    }

    if (!connection) {
        return (
            <div className="flex flex-col items-center justify-center h-[50vh] space-y-4">
                <h2 className="text-2xl font-semibold">{t('pages.deskDashboard.notFound')}</h2>
                <p className="text-muted-foreground">{t('pages.deskDashboard.notFoundDesc')}</p>
                <Button onClick={() => navigate('/desk/list')}>{t('pages.deskDashboard.backToList')}</Button>
            </div>
        )
    }

    return (
        <div className="p-6 space-y-8 max-w-5xl mx-auto">
            {/* Header Area */}
            <div className="flex items-center justify-between">
                <div className="flex items-center gap-4">
                    <Button variant="outline" size="icon" onClick={() => navigate('/desk/list')} className="shrink-0">
                        <ArrowLeft className="h-4 w-4" />
                    </Button>
                    <div>
                        <div className="flex items-center gap-2">
                            <h2 className="text-3xl font-bold tracking-tight">
                                {connection.version_info?.display_name || t('pages.deskDashboard.unnamedConnection')}
                            </h2>
                            <Badge variant="default" className="bg-green-500 hover:bg-green-600">{t('pages.deskDashboard.online')}</Badge>
                            {restricted.isRestricted && (
                                <Badge variant="outline" className="gap-1 border-amber-500 text-amber-500">
                                    <Lock className="h-3 w-3" />
                                    {t('pages.desk.restricted.indicator')}
                                </Badge>
                            )}
                        </div>
                        <p className="text-muted-foreground">
                            {t('pages.deskDashboard.connectionId')}{connection.connection_id}
                        </p>
                    </div>
                </div>
            </div>

            {/* Config Overview */}
            <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
                <Card>
                    <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
                        <CardTitle className="text-sm font-medium">{t('pages.deskDashboard.ipAddress')}</CardTitle>
                        <Globe className="h-4 w-4 text-muted-foreground" />
                    </CardHeader>
                    <CardContent>
                        <div className="text-2xl font-bold">{connection.ip || t('pages.deskDashboard.unknown')}</div>
                    </CardContent>
                </Card>
                <Card>
                    <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
                        <CardTitle className="text-sm font-medium">{t('pages.deskDashboard.os')}</CardTitle>
                        <Server className="h-4 w-4 text-muted-foreground" />
                    </CardHeader>
                    <CardContent>
                        <div className="text-2xl font-bold">{connection.version_info?.operation_system || t('pages.deskDashboard.unknown')}</div>
                    </CardContent>
                </Card>
                <Card>
                    <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
                        <CardTitle className="text-sm font-medium">{t('pages.deskDashboard.deskType')}</CardTitle>
                        <Monitor className="h-4 w-4 text-muted-foreground" />
                    </CardHeader>
                    <CardContent>
                        <div className="text-2xl font-bold">{connection.version_info?.remote_desk_type || t('pages.deskDashboard.unknown')}</div>
                    </CardContent>
                </Card>
            </div>

            {/* Quick Actions */}
            <div>
                <h3 className="text-xl font-semibold mb-4">{t('pages.deskDashboard.features')}</h3>
                <div className="grid gap-6 md:grid-cols-2 lg:grid-cols-4">
                    <Card className="hover:border-primary/50 transition-colors cursor-pointer flex flex-col" onClick={() => navigate(`/desk/${deskId}/control`)}>
                        <CardHeader>
                            <CardTitle className="flex items-center gap-2">
                                <Monitor className="h-5 w-5 text-blue-500" />
                                {t('pages.deskDashboard.remoteDesktop')}
                            </CardTitle>
                            <CardDescription>{t('pages.deskDashboard.remoteControlDesc')}</CardDescription>
                        </CardHeader>
                        <CardContent className="flex-1">
                            <ul className="text-sm text-muted-foreground space-y-2">
                                <li>• {t('pages.deskDashboard.remoteControlFeature1')}</li>
                                <li>• {t('pages.deskDashboard.remoteControlFeature2')}</li>
                                <li>• {t('pages.deskDashboard.remoteControlFeature3')}</li>
                            </ul>
                        </CardContent>
                        <CardFooter>
                            <Button className="w-full">{t('pages.deskDashboard.connect')}</Button>
                        </CardFooter>
                    </Card>

                    {restricted.capabilityVisible('allow_terminal') && (
                    <Card className="hover:border-primary/50 transition-colors cursor-pointer flex flex-col" onClick={() => navigate(`/desk/${deskId}/terminal`)}>
                        <CardHeader>
                            <CardTitle className="flex items-center gap-2">
                                <TerminalIcon className="h-5 w-5 text-green-500" />
                                {t('pages.deskDashboard.terminal')}
                            </CardTitle>
                            <CardDescription>{t('pages.deskDashboard.terminalDesc')}</CardDescription>
                        </CardHeader>
                        <CardContent className="flex-1">
                            <ul className="text-sm text-muted-foreground space-y-2">
                                <li>• {t('pages.deskDashboard.terminalFeature1')}</li>
                                <li>• {t('pages.deskDashboard.terminalFeature2')}</li>
                                <li>• {t('pages.deskDashboard.terminalFeature3')}</li>
                            </ul>
                        </CardContent>
                        <CardFooter>
                            <Button className="w-full" variant="secondary">{t('pages.deskDashboard.openTerminal')}</Button>
                        </CardFooter>
                    </Card>
                    )}

                    {restricted.capabilityVisible('allow_file_browse') && (
                    <Card className="hover:border-primary/50 transition-colors cursor-pointer flex flex-col" onClick={() => navigate(`/desk/${deskId}/files`)}>
                        <CardHeader>
                            <CardTitle className="flex items-center gap-2">
                                <Folder className="h-5 w-5 text-yellow-500" />
                                {t('pages.deskDashboard.fileManagement')}
                            </CardTitle>
                            <CardDescription>{t('pages.deskDashboard.fileManagerDesc')}</CardDescription>
                        </CardHeader>
                        <CardContent className="flex-1">
                            <ul className="text-sm text-muted-foreground space-y-2">
                                <li>• {t('pages.deskDashboard.fileManagerFeature1')}</li>
                                <li>• {t('pages.deskDashboard.fileManagerFeature2')}</li>
                                <li>• {t('pages.deskDashboard.fileManagerFeature3')}</li>
                            </ul>
                        </CardContent>
                        <CardFooter>
                            <Button className="w-full" variant="secondary">{t('pages.deskDashboard.browseFiles')}</Button>
                        </CardFooter>
                    </Card>
                    )}

                    {showAssistant && restricted.ownerPlaneVisible && (
                    <Card className="hover:border-primary/50 transition-colors cursor-pointer flex flex-col" onClick={() => navigate(`/desk/${deskId}/assistant`)}>
                        <CardHeader>
                            <CardTitle className="flex items-center gap-2">
                                <Bot className="h-5 w-5 text-violet-500" />
                                {t('pages.deskDashboard.deviceAssistant')}
                            </CardTitle>
                            <CardDescription>{t('pages.deskDashboard.deviceAssistantDesc')}</CardDescription>
                        </CardHeader>
                        <CardContent className="flex-1">
                            <ul className="text-sm text-muted-foreground space-y-2">
                                <li>• {t('pages.deskDashboard.deviceAssistantFeature1')}</li>
                                <li>• {t('pages.deskDashboard.deviceAssistantFeature2')}</li>
                                <li>• {t('pages.deskDashboard.deviceAssistantFeature3')}</li>
                            </ul>
                        </CardContent>
                        <CardFooter>
                            <Button className="w-full" variant="secondary">{t('pages.deskDashboard.openAssistant')}</Button>
                        </CardFooter>
                    </Card>
                    )}
                </div>
            </div>
        </div>
    )
}
