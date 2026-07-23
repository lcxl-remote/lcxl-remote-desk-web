import { lazy, Suspense, useCallback, useState } from "react"
import { useNavigate, useParams } from "react-router-dom"
import { ArrowLeft, Loader2, TerminalSquare } from "lucide-react"
import { useTranslation } from "react-i18next"

import { Button } from "@/components/ui/button"
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import { useDeviceId } from "@/hooks/use-device-id"
import { useListTerminal } from "@/services/hooks/terminalController/useListTerminal"

const TerminalView = lazy(() =>
    import("./terminal-session").then((module) => ({
        default: module.TerminalView,
    })),
)

type TerminalSessionLauncherProps = {
    orgId?: number
}

function TerminalLoading() {
    return (
        <div className="flex h-full items-center justify-center">
            <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
        </div>
    )
}

export default function TerminalSessionLauncher({
    orgId,
}: TerminalSessionLauncherProps = {}) {
    const { id: connectionId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    const deviceId = useDeviceId(connectionId)
    const { data: terminalList, isLoading } = useListTerminal(
        connectionId || '',
        deviceId ? { device_id: deviceId } : undefined,
    )
    const [selectedCommand, setSelectedCommand] = useState("")

    const handleTerminalClose = useCallback(() => {
        setSelectedCommand("")
    }, [])

    if (isLoading) {
        return <TerminalLoading />
    }

    if (selectedCommand && connectionId) {
        return (
            <Suspense fallback={<TerminalLoading />}>
                <TerminalView
                    connectionId={connectionId}
                    deviceId={deviceId}
                    command={selectedCommand}
                    onClose={handleTerminalClose}
                    orgId={orgId}
                />
            </Suspense>
        )
    }

    const commands = terminalList?.commands || []

    return (
        <div className="flex h-full items-center justify-center bg-muted/40 p-4 relative">
            <div className="absolute top-4 left-4">
                <Button
                    variant="outline"
                    size="sm"
                    onClick={() => navigate(`/desk/${connectionId}`)}
                >
                    <ArrowLeft className="mr-2 h-4 w-4" />
                    {t('pages.deskTerminal.dashboard')}
                </Button>
            </div>
            <Card className="w-full max-w-md">
                <CardHeader>
                    <CardTitle className="flex items-center gap-2">
                        <TerminalSquare className="h-6 w-6" />
                        {t('pages.deskTerminal.title')}
                    </CardTitle>
                    <CardDescription>
                        {t('pages.deskTerminal.selectShell')}
                    </CardDescription>
                </CardHeader>
                <CardContent className="grid gap-4">
                    <div className="grid gap-2">
                        <Label htmlFor="shell">
                            {t('pages.deskTerminal.shellCommand')}
                        </Label>
                        <select
                            id="shell"
                            className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background file:border-0 file:bg-transparent file:text-sm file:font-medium placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
                            value={selectedCommand}
                            onChange={(event) =>
                                setSelectedCommand(event.target.value)
                            }
                        >
                            <option value="" disabled>
                                {t('pages.deskTerminal.shellPlaceholder')}
                            </option>
                            {commands.map((command: string[]) => {
                                const value = command.join(',')
                                return (
                                    <option key={value} value={value}>
                                        {command[0]}
                                    </option>
                                )
                            })}
                        </select>
                    </div>
                    <Button disabled={!selectedCommand}>
                        {t('pages.deskTerminal.connect')}
                    </Button>
                </CardContent>
            </Card>
        </div>
    )
}
