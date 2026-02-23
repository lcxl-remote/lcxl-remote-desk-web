
import { useEffect, useRef, useState, useCallback } from "react"
import { useParams, useNavigate } from "react-router-dom"
import { Terminal } from "xterm"
import { FitAddon } from "xterm-addon-fit"
import { WebLinksAddon } from "xterm-addon-web-links"
import "xterm/css/xterm.css"
import { useTranslation } from "react-i18next"
import { Loader2, TerminalSquare, ArrowLeft } from "lucide-react"
import { useListTerminal } from "@/services/hooks/undefinedController/useListTerminal"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import { v4 } from "uuid"

// Legacy Signaling Constants
const SIGNALING_TYPE_CODE_REPLY = 10011
const SIGNALING_TYPE_CODE_DATA = 10008
const SIGNALING_TYPE_CODE_RESIZE = 10009
const SIGNALING_TYPE_CODE_TERMINAL_STARTED = 10013
const SIGNALING_TYPE_CODE_TERMINAL_CLOSED = 10014

function TerminalView({ sessionId, command, onClose }: { sessionId: string; command: string; onClose: () => void }) {
    const terminalRef = useRef<HTMLDivElement>(null)
    const [isConnected, setIsConnected] = useState(false)
    const xtermRef = useRef<Terminal | null>(null)
    const fitAddonRef = useRef<FitAddon | null>(null)
    const socketRef = useRef<WebSocket | null>(null)
    const resizeObserverRef = useRef<ResizeObserver | null>(null)
    const terminalStarted = useRef<boolean>(false)

    useEffect(() => {
        if (!terminalRef.current || !sessionId) return

        // Initialize xterm
        const term = new Terminal({
            cursorBlink: true,
            theme: {
                background: '#1e1e1e',
            },
            fontFamily: 'Menlo, Monaco, "Courier New"',
            fontSize: 14,
            allowProposedApi: true
        })

        const fitAddon = new FitAddon()
        fitAddonRef.current = fitAddon
        term.loadAddon(fitAddon)
        term.loadAddon(new WebLinksAddon())

        term.open(terminalRef.current)
        xtermRef.current = term

        // Safe fit function
        const safeFit = () => {
            if (terminalRef.current && terminalRef.current.clientWidth > 0 && terminalRef.current.clientHeight > 0) {
                try {
                    fitAddon.fit()
                } catch (e) {
                    console.warn("Fit error ignored:", e)
                }
            }
        }

        // Initial fit delay
        setTimeout(safeFit, 100)

        // Connect to WebSocket
        let ws: WebSocket | null = null;
        let connectTimer: number;

        try {
            const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
            const host = window.location.host;
            const url = new URL(`${protocol}//${host}/api/desk/terminal/${sessionId}`);

            // Add command param
            // Legacy logic: command = encodeURIComponent(select_command.join(","))
            // The command prop passed here is already the "joined" string or the raw value?
            // Let's assume the parent passes the ready-to-use command string or array.
            // If the user selected a JSON stringified array, we need to parse and join.
            // But let's handle the parsing in the parent.
            url.searchParams.append("command", command);

            const connectWS = () => {
                try {
                    ws = new WebSocket(url.toString());
                    socketRef.current = ws;

            ws.onopen = () => {
                console.log("Terminal WebSocket connected")
                if (xtermRef.current) setIsConnected(true)
                term.write('\r\n\x1b[32mConnected to remote terminal\x1b[0m\r\n')
                safeFit()

                // Send initial size
                sendResize({ rows: term.rows, cols: term.cols })
            }

            ws.onmessage = (event) => {
                if (typeof event.data === 'string') {
                    try {
                        const msg = JSON.parse(event.data)
                        if (msg.signaling_type === SIGNALING_TYPE_CODE_REPLY) {
                            term.write(msg.signaling_data.content)
                        } else if (msg.signaling_type === SIGNALING_TYPE_CODE_TERMINAL_STARTED) {
                            console.log("terminal started")
                            terminalStarted.current = true
                            // Send resize again after started to be safe
                            sendResize({ rows: term.rows, cols: term.cols })
                        } else if (msg.signaling_type === SIGNALING_TYPE_CODE_TERMINAL_CLOSED) {
                            console.log("terminal closed")
                            term.write('\r\n\x1b[31mTerminal connection closed by server.\x1b[0m\r\n')
                            ws?.close()
                        }
                    } catch (e) {
                        console.error('Error parsing JSON message:', e)
                        term.write(event.data)
                    }
                } else {
                    const reader = new FileReader();
                    reader.onload = () => {
                        if (reader.result instanceof ArrayBuffer) {
                            term.write(new Uint8Array(reader.result))
                        } else if (typeof reader.result === 'string') {
                            term.write(reader.result)
                        }
                    };
                    reader.readAsArrayBuffer(event.data);
                }
            }

            ws.onclose = () => {
                console.log("Terminal WebSocket disconnected")
                if (xtermRef.current) {
                    setIsConnected(false)
                    term.write('\r\n\x1b[31mDisconnected. Returning to shell selection...\x1b[0m\r\n')
                }
                terminalStarted.current = false

                // Trigger onClose after a brief delay so the user sees the disconnect message
                setTimeout(() => {
                    onClose()
                }, 1000)
            }

                    ws.onerror = (error) => {
                        console.error("Terminal WebSocket error", error)
                    }
                } catch (e) {
                    console.error("WebSocket init error", e)
                }
            };

            connectTimer = window.setTimeout(connectWS, 300);

            term.onData(data => {
                if (socketRef.current && socketRef.current.readyState === WebSocket.OPEN && terminalStarted.current) {
                    const signal = {
                        request_id: v4(),
                        signaling_type: SIGNALING_TYPE_CODE_DATA,
                        to_session_id: sessionId,
                        signaling_data: { content: data },
                    };
                    socketRef.current.send(JSON.stringify(signal));
                }
            })

            const sendResize = (size: { cols: number, rows: number }) => {
                if (socketRef.current && socketRef.current.readyState === WebSocket.OPEN && terminalStarted.current) {
                    const signal = {
                        request_id: v4(),
                        signaling_type: SIGNALING_TYPE_CODE_RESIZE,
                        to_session_id: sessionId,
                        signaling_data: { rows: size.rows, cols: size.cols },
                    };
                    socketRef.current.send(JSON.stringify(signal));
                }
            }

            term.onResize(sendResize)

        } catch (e) {
            console.error("WebSocket URL init error", e)
        }

        // Handle resize using ResizeObserver with debounce
        let resizeTimeout: ReturnType<typeof setTimeout>;
        const resizeObserver = new ResizeObserver(() => {
            clearTimeout(resizeTimeout)
            resizeTimeout = setTimeout(() => {
                if (xtermRef.current) {
                    safeFit()
                }
            }, 100)
        })

        resizeObserver.observe(terminalRef.current)
        resizeObserverRef.current = resizeObserver

        return () => {
            console.log("Cleaning up terminal session")
            clearTimeout(connectTimer);
            if (ws) {
                // Remove onclose handler to avoid triggering onClose navigate when unmounting component normally
                ws.onclose = null
                ws.close()
                ws = null
            }
            if (resizeObserverRef.current) {
                resizeObserverRef.current.disconnect()
            }
            clearTimeout(resizeTimeout)

            xtermRef.current = null
            term.dispose()
        }
    }, [sessionId, command, onClose])

    return (
        <div className="h-screen w-full flex flex-col bg-[#1e1e1e] overflow-hidden relative">
            <div className="absolute top-2 right-4 z-10 flex gap-2">
                <Button
                    variant="secondary"
                    size="sm"
                    className="opacity-50 hover:opacity-100 transition-opacity"
                    onClick={() => {
                        if (socketRef.current) {
                            socketRef.current.close()
                        } else {
                            onClose()
                        }
                    }}
                >
                    <TerminalSquare className="h-4 w-4 mr-2" />
                    Switch Shell
                </Button>
            </div>
            <div className="flex-1 w-full p-2 overflow-hidden relative" ref={terminalRef} />
            {!isConnected && (
                <div className="absolute top-1/2 left-1/2 transform -translate-x-1/2 -translate-y-1/2 text-white flex items-center gap-2 pointer-events-none">
                    <Loader2 className="h-6 w-6 animate-spin" />
                    <span>Connecting...</span>
                </div>
            )}
        </div>
    )
}

export default function TerminalSession() {
    const { id: sessionId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    const { data: terminalList, isLoading } = useListTerminal(sessionId || '')
    const [selectedCommand, setSelectedCommand] = useState<string>("")

    const handleTerminalClose = useCallback(() => {
        setSelectedCommand("");
    }, []);

    if (isLoading) {
        return (
            <div className="flex h-screen items-center justify-center">
                <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
            </div>
        )
    }

    if (selectedCommand && sessionId) {
        return <TerminalView
            sessionId={sessionId}
            command={selectedCommand}
            onClose={handleTerminalClose}
        />
    }

    const commands = terminalList?.commands || []

    return (
        <div className="flex h-screen items-center justify-center bg-muted/40 p-4 relative">
            <div className="absolute top-4 left-4">
                <Button variant="outline" size="sm" onClick={() => navigate(`/desk/${sessionId}`)}>
                    <ArrowLeft className="mr-2 h-4 w-4" />
                    Dashboard
                </Button>
            </div>
            <Card className="w-full max-w-md">
                <CardHeader>
                    <CardTitle className="flex items-center gap-2">
                        <TerminalSquare className="h-6 w-6" />
                        {t('pages.deskTerminal.title', 'Terminal Session')}
                    </CardTitle>
                    <CardDescription>
                        {t('pages.deskTerminal.selectShell', 'Select a shell to start the session')}
                    </CardDescription>
                </CardHeader>
                <CardContent className="grid gap-4">
                    <div className="grid gap-2">
                        <Label htmlFor="shell">Shell Command</Label>
                        <select
                            id="shell"
                            className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background file:border-0 file:bg-transparent file:text-sm file:font-medium placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
                            value={selectedCommand}
                            onChange={(e) => setSelectedCommand(e.target.value)}
                        >
                            <option value="" disabled>Select a shell...</option>
                            {commands.map((cmd: string[], i: number) => {
                                // cmd is string[], join with comma for display and usage
                                // Legacy used JSON.stringify for value, but why?
                                // "command" param in URL is comma-separated string.
                                // So we can just join it here.
                                const value = cmd.join(',')
                                return (
                                    <option key={i} value={value}>
                                        {cmd[0]}
                                    </option>
                                )
                            })}
                        </select>
                    </div>
                    <Button
                        disabled={!selectedCommand}
                        onClick={() => {
                            // Trigger re-render with selected command
                            // State update handles it
                        }}
                    >
                        {t('pages.deskTerminal.connect', 'Connect')}
                    </Button>
                </CardContent>
            </Card>
        </div>
    )
}
