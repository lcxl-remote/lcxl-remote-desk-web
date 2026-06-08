
import { useTranslation } from "react-i18next"
import { useNavigate } from "react-router-dom"
import { Power, RefreshCw } from "lucide-react"

import { Button } from "@/components/ui/button"
import {
    Card,
    CardContent,
    CardDescription,
    CardFooter,
    CardHeader,
    CardTitle,
} from "@/components/ui/card"
import { Badge } from "@/components/ui/badge"
import { useListConnections } from "@/services/hooks/connectionController/useListConnections"
import { Skeleton } from "@/components/ui/skeleton"

export default function DeskList() {
    const { t } = useTranslation()
    const navigate = useNavigate()
    const { data: connections, isLoading, refetch } = useListConnections()

    // Helper to handle navigation to desk
    const handleConnect = (id: string) => {
        navigate(`/desk/${id}`)
    }

    if (isLoading) {
        return (
            <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
                {[...Array(6)].map((_, i) => (
                    <Skeleton key={i} className="h-[200px] w-full rounded-xl" />
                ))}
            </div>
        )
    }

    return (
        <div className="space-y-4">
            <div className="flex items-center justify-between">
                <h2 className="text-3xl font-bold tracking-tight">{t('menu.desk', 'Desk List')}</h2>
                <div className="flex items-center gap-2">
                    <Button variant="outline" size="icon" onClick={() => refetch()}>
                        <RefreshCw className="h-4 w-4" />
                    </Button>
                </div>
            </div>

            <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
                {connections?.length === 0 && (
                    <div className="col-span-full text-center text-muted-foreground py-10">
                        {t('pages.deskList.empty', 'No connections found')}
                    </div>
                )}
                {connections?.map((connection: any) => (
                    <Card key={connection.connection_id} className="overflow-hidden">
                        <CardHeader className="flex flex-row items-start bg-muted/50">
                            <div className="grid gap-0.5">
                                <CardTitle className="group flex items-center gap-2 text-lg">
                                    {connection.version_info?.display_name || t('pages.deskList.unnamedConnection', 'Unnamed Connection')}
                                    <Badge variant="default" className="bg-green-500 hover:bg-green-600">{t('pages.searchTable.nameStatus.online', 'Online')}</Badge>
                                </CardTitle>
                                <CardDescription>
                                    {connection.version_info?.operation_system} - {connection.version_info?.remote_desk_type}
                                </CardDescription>
                            </div>
                        </CardHeader>
                        <CardContent className="p-6 text-sm">
                            <div className="grid gap-3">
                                <div className="font-semibold">{t('pages.searchTable.detail', 'Details')}</div>
                                <ul className="grid gap-3">
                                    <li className="flex items-center justify-between">
                                        <span className="text-muted-foreground">{t('pages.deskList.connectionId', 'Connection ID')}</span>
                                        <span className="font-mono text-xs">{connection.connection_id}</span>
                                    </li>
                                    <li className="flex items-center justify-between">
                                        <span className="text-muted-foreground">{t('pages.deskList.ip', 'IP')}</span>
                                        <span>{connection.ip || t('pages.desk.unknown', 'Unknown')}</span>
                                    </li>
                                    <li className="flex items-center justify-between">
                                        <span className="text-muted-foreground">{t('pages.deskList.platform', 'Platform')}</span>
                                        <span>{connection.version_info?.operation_system || t('pages.desk.unknown', 'Unknown')}</span>
                                    </li>
                                </ul>
                            </div>
                        </CardContent>
                        <CardFooter className="flex flex-row items-center justify-end border-t bg-muted/50 px-6 py-3">
                            <Button size="sm" onClick={() => handleConnect(connection.connection_id)}>
                                <Power className="mr-2 h-4 w-4" /> {t('pages.deskList.connect', 'Connect')}
                            </Button>
                        </CardFooter>
                    </Card>
                ))}
            </div>
        </div>
    )
}
