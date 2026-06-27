
import { useEffect, useRef, useState, useCallback } from "react"
import { useParams, useNavigate } from "react-router-dom"
import { Terminal } from "@xterm/xterm"
import { FitAddon } from "@xterm/addon-fit"
import { WebLinksAddon } from "@xterm/addon-web-links"
import "@xterm/xterm/css/xterm.css"
import { useTranslation } from "react-i18next"
import { Loader2, TerminalSquare, ArrowLeft } from "lucide-react"
import { Sparkles, WandSparkles } from "lucide-react"
import { useListTerminal } from "@/services/hooks/terminalController/useListTerminal"
import { useDeviceId } from "@/hooks/use-device-id"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import { v4 } from "uuid"
import { useDeskSignaling } from "../desk/use-desk-signaling"
import { useTerminalCopilot, type TerminalCopilotMode, type TerminalContext } from "./use-terminal-copilot"
import { TerminalCopilotPanel } from "./terminal-copilot-panel"
import { useConfirmExec } from "../exec/use-confirm-exec"
import {
    useTerminalComplete,
    pickLocalGhost,
    commonCommandsFor,
    type TerminalCompletionContext,
} from "./use-terminal-complete"

// Max bytes of recent terminal scrollback kept as a non-authoritative copilot
// prompt hint (the server re-redacts and re-caps it). Bounded so the ring buffer
// never grows without limit on a chatty session.
const COPILOT_RECENT_OUTPUT_LIMIT = 8_192

// Legacy Signaling Constants
const SIGNALING_TYPE_CODE_REPLY = 10011
const SIGNALING_TYPE_CODE_DATA = 10008
const SIGNALING_TYPE_CODE_RESIZE = 10009
const SIGNALING_TYPE_CODE_TERMINAL_STARTED = 10013
const SIGNALING_TYPE_CODE_TERMINAL_CLOSED = 10014
const SIGNALING_TYPE_CODE_HEARTBEAT = 1
const TERMINAL_HEARTBEAT_INTERVAL_MS = 30_000

function TerminalView({ connectionId, deviceId, command, onClose }: { connectionId: string; deviceId?: string; command: string; onClose: () => void }) {
    const { t } = useTranslation()
    const terminalRef = useRef<HTMLDivElement>(null)
    const [isConnected, setIsConnected] = useState(false)
    const xtermRef = useRef<Terminal | null>(null)
    const fitAddonRef = useRef<FitAddon | null>(null)
    const socketRef = useRef<WebSocket | null>(null)
    const resizeObserverRef = useRef<ResizeObserver | null>(null)
    const terminalStarted = useRef<boolean>(false)
    const heartbeatTimerRef = useRef<number | null>(null)

    // Copilot: a control-plane signaling connection (separate from the terminal
    // I/O WS above) plus a bounded ring buffer of recent output and the last
    // submitted command line, all fed to the copilot as non-authoritative hints.
    const { subscribe, sendMessage } = useDeskSignaling(connectionId)
    const copilot = useTerminalCopilot({ connectionId, subscribe, sendMessage })
    // Confirmed execution of an operator-promoted copilot suggestion: ConfirmExec
    // -> ExecPreview -> ResolveExec -> ExecResult, keyed by suggestion index. The
    // host re-classifies the command and gates it on the device execution ceiling;
    // the browser only relays and renders.
    const exec = useConfirmExec({ deskId: connectionId, subscribe, sendMessage })
    const complete = useTerminalComplete({ connectionId, subscribe, sendMessage })
    const [showCopilot, setShowCopilot] = useState(false)
    const recentOutputRef = useRef<string>("")
    const lastCommandRef = useRef<string>("")
    const inputLineRef = useRef<string>("")

    // AI command completion (ghost text). A local toggle lets the operator silence
    // it; when on, the layered logic is L1 instant (recent-command history) plus a
    // debounced L2 AI ask. The accepted suffix is filled (never auto-run).
    const [completionEnabled, setCompletionEnabled] = useState(true)
    const completionEnabledRef = useRef(completionEnabled)
    completionEnabledRef.current = completionEnabled
    const historyRef = useRef<string[]>([])
    const currentPrefixRef = useRef<string>("")
    const [ghost, setGhost] = useState<{ suffix: string; note: string; source: 'ai' | 'history' } | null>(null)
    const ghostRef = useRef(ghost)
    ghostRef.current = ghost

    // Inject text into the shell input WITHOUT a trailing Enter — equivalent to the
    // operator typing it; they press Enter themselves. The AI path never runs a
    // command automatically (suggest-only invariant).
    const fillCommand = useCallback((text: string) => {
        const ws = socketRef.current
        if (ws && ws.readyState === WebSocket.OPEN && terminalStarted.current) {
            ws.send(JSON.stringify({
                request_id: v4(),
                signaling_type: SIGNALING_TYPE_CODE_DATA,
                to_connection_id: connectionId,
                signaling_data: { content: text },
            }))
        }
        xtermRef.current?.focus()
    }, [connectionId])

    // Derive the (non-authoritative) shell/os hint from the selected command.
    const buildContext = useCallback((mode: TerminalCopilotMode): TerminalContext => {
        const shell = (command.split(",")[0] || "").split(/[\\/]/).pop() || command
        const isWindows = /cmd|powershell|pwsh/i.test(shell)
        const recent = recentOutputRef.current.slice(-COPILOT_RECENT_OUTPUT_LIMIT)
        return {
            os: isWindows ? "windows" : "linux",
            shell,
            recent_output: recent,
            last_command: lastCommandRef.current || undefined,
            // In explain mode the recent scrollback IS the error passage the
            // operator is asking about; the server caps it again.
            error_text: mode === "explain_error" ? recent : undefined,
        }
    }, [command])

    const askCopilot = useCallback((mode: TerminalCopilotMode, question: string) => {
        copilot.ask({ mode, question: question || undefined, context: buildContext(mode) })
    }, [copilot, buildContext])

    // The (non-authoritative) environment hint for a completion ask.
    const completeContext = useCallback((): TerminalCompletionContext => {
        const shell = (command.split(",")[0] || "").split(/[\\/]/).pop() || command
        const isWindows = /cmd|powershell|pwsh/i.test(shell)
        return {
            os: isWindows ? "windows" : "linux",
            shell,
            recent_output: recentOutputRef.current.slice(-COPILOT_RECENT_OUTPUT_LIMIT),
        }
    }, [command])

    // Called whenever the live input line changes. Drives the layered completion:
    // an instant L1 suggestion from recent-command history, plus a debounced L2 AI
    // ask. A settled line (Enter) clears the ghost and records the command.
    const onInputChanged = useCallback((line: string, settled: boolean) => {
        currentPrefixRef.current = line
        if (settled) {
            if (line) historyRef.current = [...historyRef.current.slice(-99), line]
            setGhost(null)
            complete.clear()
            return
        }
        if (!completionEnabledRef.current || !line) {
            setGhost(null)
            complete.clear()
            return
        }
        // L1: instant, zero-latency history match, then the known-command corpus.
        const shell = (command.split(",")[0] || "").split(/[\\/]/).pop() || command
        const local = pickLocalGhost(line, historyRef.current, commonCommandsFor(shell))
        setGhost(local ? { suffix: local, note: '', source: 'history' } : null)
        // L2: debounced AI ask (its result upgrades the ghost when it lands).
        complete.requestCompletion(line, completeContext())
    }, [complete, completeContext, command])
    const onInputChangedRef = useRef(onInputChanged)
    onInputChangedRef.current = onInputChanged

    // Accept the current ghost: fill its suffix (no Enter — suggest-only) and fold
    // it into the tracked input line so the next keystroke continues from there.
    const acceptGhost = useCallback(() => {
        const g = ghostRef.current
        if (!g) return false
        fillCommand(g.suffix)
        inputLineRef.current += g.suffix
        currentPrefixRef.current = inputLineRef.current
        setGhost(null)
        complete.clear()
        return true
    }, [fillCommand, complete])
    const acceptGhostRef = useRef(acceptGhost)
    acceptGhostRef.current = acceptGhost

    // When an AI result lands for the prefix still in the input, upgrade the ghost
    // from the L1 history guess to the (richer) AI suggestion.
    useEffect(() => {
        if (!completionEnabledRef.current) return
        if (complete.best && complete.completionPrefix === currentPrefixRef.current) {
            setGhost({ suffix: complete.best.completion, note: complete.best.note, source: 'ai' })
        }
    }, [complete.best, complete.completionPrefix])

    useEffect(() => {
        if (!terminalRef.current || !connectionId) return

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
            const url = new URL(`${protocol}//${host}/api/desk/terminal/${connectionId}`);

            // Add command param
            // Legacy logic: command = encodeURIComponent(select_command.join(","))
            // The command prop passed here is already the "joined" string or the raw value?
            // Let's assume the parent passes the ready-to-use command string or array.
            // If the user selected a JSON stringified array, we need to parse and join.
            // But let's handle the parsing in the parent.
            url.searchParams.append("command", command);
            // device_id lets the enterprise manager route the WS to the instance
            // owning the connection; the OSS signal leaves it unset and routes by
            // the path connection_id (dual-target wire model).
            if (deviceId) url.searchParams.append("device_id", deviceId);

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

                        // Start heartbeat to keep connection alive through reverse proxy
                        if (heartbeatTimerRef.current !== null) {
                            clearInterval(heartbeatTimerRef.current)
                        }
                        heartbeatTimerRef.current = window.setInterval(() => {
                            if (socketRef.current && socketRef.current.readyState === WebSocket.OPEN) {
                                const heartbeat = {
                                    request_id: v4(),
                                    signaling_type: SIGNALING_TYPE_CODE_HEARTBEAT,
                                    signaling_data: null,
                                };
                                socketRef.current.send(JSON.stringify(heartbeat));
                            }
                        }, TERMINAL_HEARTBEAT_INTERVAL_MS)
                    }

                    ws.onmessage = (event) => {
                        if (typeof event.data === 'string') {
                            try {
                                const msg = JSON.parse(event.data)
                                if (msg.signaling_type === SIGNALING_TYPE_CODE_REPLY) {
                                    const content = msg.signaling_data.content
                                    term.write(content)
                                    // Tap a bounded ring buffer of recent output for the copilot hint.
                                    if (typeof content === 'string') {
                                        recentOutputRef.current = (recentOutputRef.current + content)
                                            .slice(-COPILOT_RECENT_OUTPUT_LIMIT)
                                    }
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
                        to_connection_id: connectionId,
                        signaling_data: { content: data },
                    };
                    socketRef.current.send(JSON.stringify(signal));
                    // Track the last submitted command line (a non-authoritative copilot
                    // hint). A CR/LF settles the current line; backspace pops; printable
                    // characters accumulate. Control sequences are otherwise ignored.
                    let settled = false
                    for (const ch of data) {
                        if (ch === '\r' || ch === '\n') {
                            const line = inputLineRef.current.trim()
                            if (line) lastCommandRef.current = line
                            inputLineRef.current = ''
                            settled = true
                        } else if (ch === '\x7f' || ch === '\b') {
                            inputLineRef.current = inputLineRef.current.slice(0, -1)
                        } else if (ch >= ' ') {
                            inputLineRef.current += ch
                        }
                    }
                    // Drive the layered command completion off the live input line.
                    onInputChangedRef.current(
                        settled ? lastCommandRef.current : inputLineRef.current,
                        settled,
                    )
                }
            })

            // Capture Tab to accept the current ghost completion (filling its suffix
            // without a trailing Enter). With no ghost, Tab passes through to the PTY
            // so native shell completion still works.
            term.attachCustomKeyEventHandler((e) => {
                if (e.type === 'keydown' && e.key === 'Tab' && ghostRef.current) {
                    acceptGhostRef.current()
                    return false
                }
                return true
            })

            const sendResize = (size: { cols: number, rows: number }) => {
                if (socketRef.current && socketRef.current.readyState === WebSocket.OPEN && terminalStarted.current) {
                    const signal = {
                        request_id: v4(),
                        signaling_type: SIGNALING_TYPE_CODE_RESIZE,
                        to_connection_id: connectionId,
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
            if (heartbeatTimerRef.current !== null) {
                clearInterval(heartbeatTimerRef.current)
                heartbeatTimerRef.current = null
            }
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
    }, [connectionId, deviceId, command, onClose])

    return (
        <div className="h-full w-full flex bg-[#1e1e1e] overflow-hidden">
            <div className="relative flex-1 flex flex-col overflow-hidden">
                <div className="absolute top-2 right-4 z-10 flex gap-2">
                    <Button
                        variant="secondary"
                        size="sm"
                        className={`transition-opacity ${completionEnabled ? 'opacity-90' : 'opacity-40'} hover:opacity-100`}
                        onClick={() => {
                            setCompletionEnabled((v) => {
                                if (v) {
                                    setGhost(null)
                                    complete.clear()
                                }
                                return !v
                            })
                        }}
                        title={t('pages.deskTerminal.completion.toggleHint')}
                    >
                        <WandSparkles className="h-4 w-4 mr-2" />
                        {t('pages.deskTerminal.completion.title')}
                    </Button>
                    <Button
                        variant="secondary"
                        size="sm"
                        className="opacity-50 hover:opacity-100 transition-opacity"
                        onClick={() => setShowCopilot((v) => !v)}
                    >
                        <Sparkles className="h-4 w-4 mr-2" />
                        {t('pages.deskTerminal.copilot.title')}
                    </Button>
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
                <div className="flex-1 w-full p-2 overflow-hidden relative">
                    <div className="absolute inset-2 overflow-hidden" ref={terminalRef} />
                </div>
                {!isConnected && (
                    <div className="absolute top-1/2 left-1/2 transform -translate-x-1/2 -translate-y-1/2 text-white flex items-center gap-2 pointer-events-none">
                        <Loader2 className="h-6 w-6 animate-spin" />
                        <span>Connecting...</span>
                    </div>
                )}
                {completionEnabled && ghost && (
                    <div className="absolute bottom-2 left-4 right-4 z-10 flex items-center gap-2 rounded bg-black/60 px-3 py-1.5 text-xs text-gray-300 pointer-events-none">
                        <WandSparkles className="h-3.5 w-3.5 shrink-0 text-sky-400" />
                        <span className="font-mono text-gray-500 truncate">
                            {currentPrefixRef.current}
                            <span className="text-sky-300">{ghost.suffix}</span>
                        </span>
                        {ghost.note && (
                            <span className="truncate text-gray-400">— {ghost.note}</span>
                        )}
                        <span className="ml-auto shrink-0 rounded border border-gray-600 px-1.5 py-0.5 text-[10px] text-gray-400">
                            {t('pages.deskTerminal.completion.acceptHint')}
                        </span>
                    </div>
                )}
            </div>
            {showCopilot && (
                <TerminalCopilotPanel
                    state={copilot.state}
                    onAsk={askCopilot}
                    onReset={copilot.reset}
                    onClose={() => setShowCopilot(false)}
                    onFill={fillCommand}
                    exec={exec}
                />
            )}
        </div>
    )
}

export default function TerminalSession() {
    const { id: connectionId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()
    const deviceId = useDeviceId(connectionId)
    const { data: terminalList, isLoading } = useListTerminal(
        connectionId || '',
        deviceId ? { device_id: deviceId } : undefined,
    )
    const [selectedCommand, setSelectedCommand] = useState<string>("")

    const handleTerminalClose = useCallback(() => {
        setSelectedCommand("");
    }, []);

    if (isLoading) {
        return (
            <div className="flex h-full items-center justify-center">
                <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
            </div>
        )
    }

    if (selectedCommand && connectionId) {
        return <TerminalView
            connectionId={connectionId}
            deviceId={deviceId}
            command={selectedCommand}
            onClose={handleTerminalClose}
        />
    }

    const commands = terminalList?.commands || []

    return (
        <div className="flex h-full items-center justify-center bg-muted/40 p-4 relative">
            <div className="absolute top-4 left-4">
                <Button variant="outline" size="sm" onClick={() => navigate(`/desk/${connectionId}`)}>
                    <ArrowLeft className="mr-2 h-4 w-4" />
                    Dashboard
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
                        {t('pages.deskTerminal.connect')}
                    </Button>
                </CardContent>
            </Card>
        </div>
    )
}
