
import { useEffect, useRef, useState, useCallback } from "react"
import { Terminal } from "@xterm/xterm"
import { FitAddon } from "@xterm/addon-fit"
import { WebLinksAddon } from "@xterm/addon-web-links"
import "@xterm/xterm/css/xterm.css"
import { useTranslation } from "react-i18next"
import { Loader2, TerminalSquare } from "lucide-react"
import { Sparkles, WandSparkles } from "lucide-react"
import { readSessionGrant } from "@/features/desk/session-grant"
import { Button } from "@/components/ui/button"
import { v4 } from "uuid"
import { useDeskSignaling } from "../desk/use-desk-signaling"
import { useTerminalCopilot, type TerminalCopilotMode, type TerminalContext } from "./use-terminal-copilot"
import { TerminalCopilotPanel } from "./terminal-copilot-panel"
import { AiGeneratedMark, type AiProvenance } from "@/components/ai-generated-mark"
import { ModelSelector } from "../desk/model-selector"
import { useConfirmExec } from "../exec/use-confirm-exec"
import { deskErrorCodeEnum } from "@/services/types"
import { AdmissionRetrySchedule } from "../desk/admission-retry"
import {
    useTerminalComplete,
    pickLocalGhost,
    commonCommandsFor,
    type TerminalCompletionContext,
} from "./use-terminal-complete"
import { terminalWelcomeBanner } from "./terminal-welcome"
import { useTerminalSessionGuard } from "./use-terminal-session-guard"

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
const SIGNALING_TYPE_CODE_SEND_HEARTBEAT = 1
const SIGNALING_TYPE_CODE_ERROR = -1
const TERMINAL_HEARTBEAT_INTERVAL_MS = 30_000

export function TerminalView({ connectionId, deviceId, command, onClose, orgId }: { connectionId: string; deviceId?: string; command: string; onClose: () => void; orgId?: number }) {
    const { t } = useTranslation()
    const terminalRef = useRef<HTMLDivElement>(null)
    const [isConnected, setIsConnected] = useState(false)
    const xtermRef = useRef<Terminal | null>(null)
    const fitAddonRef = useRef<FitAddon | null>(null)
    const socketRef = useRef<WebSocket | null>(null)
    const resizeObserverRef = useRef<ResizeObserver | null>(null)
    const terminalStarted = useRef<boolean>(false)
    const heartbeatTimerRef = useRef<number | null>(null)
    const { markStarted: markTerminalStarted, markClosed: markTerminalClosed } = useTerminalSessionGuard()

    // Copilot: a control-plane signaling connection (separate from the terminal
    // I/O WS above) plus a bounded ring buffer of recent output and the last
    // submitted command line, all fed to the copilot as non-authoritative hints.
    const { subscribe, sendMessage } = useDeskSignaling()
    const copilot = useTerminalCopilot({ connectionId, subscribe, sendMessage })
    // Confirmed execution of an operator-promoted copilot suggestion: ConfirmExec
    // -> ExecPreview -> ResolveExec -> ExecResult, keyed by suggestion index. The
    // host re-classifies the command and gates it on the device execution ceiling;
    // the browser only relays and renders.
    const exec = useConfirmExec({ deskId: connectionId, subscribe, sendMessage, orgId })
    // The manager-selected completion model, or null when the selector is hidden
    // (open-source signal) — then no `model_id` rides the completion ask.
    const [completionModelId, setCompletionModelId] = useState<number | null>(null)
    const complete = useTerminalComplete({
        connectionId,
        subscribe,
        sendMessage,
        modelId: completionModelId,
        orgId,
    })
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
    const [ghost, setGhost] = useState<{ suffix: string; note: string; source: 'ai' | 'history'; provenance?: AiProvenance | null } | null>(null)
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

    const askCopilot = useCallback((mode: TerminalCopilotMode, question: string, modelId: number | null) => {
        copilot.ask({ mode, question: question || undefined, context: buildContext(mode), modelId, orgId })
    }, [copilot, buildContext, orgId])

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
            setGhost({ suffix: complete.best.completion, note: complete.best.note, source: 'ai', provenance: complete.provenance })
        }
    }, [complete.best, complete.completionPrefix, complete.provenance])

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

        safeFit()
        term.write(terminalWelcomeBanner(term.cols))

        // Initial fit delay
        setTimeout(safeFit, 100)

        // Connect to WebSocket
        let ws: WebSocket | null = null;
        let connectTimer: number;
        let admissionRetryTimer: number | null = null;
        let disposed = false;
        const admissionRetry = new AdmissionRetrySchedule();

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
            // A capability-scoped code-session carries the grant it redeemed so the
            // central can stamp the terminal with the code's ceiling (the terminal WS
            // is a distinct connection that never does a RequestRemoteAccess). An owner
            // session has no grant and omits it (stamped with full control).
            const grant = readSessionGrant(connectionId);
            if (grant?.grantSessionId) {
                url.searchParams.append("grant_session_id", grant.grantSessionId);
            }

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
                                    signaling_type: SIGNALING_TYPE_CODE_SEND_HEARTBEAT,
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
                                if (
                                    msg.signaling_type === SIGNALING_TYPE_CODE_ERROR
                                    && !terminalStarted.current
                                    && msg.response_state?.error_code === deskErrorCodeEnum.ACTION_NEED_RETRY
                                ) {
                                    const delay = admissionRetry.nextDelay();
                                    ws!.onclose = null;
                                    ws!.close();
                                    if (delay === null) {
                                        term.write(`\r\n\x1b[33m${t('pages.desk.admissionRetry.exhausted')}\x1b[0m\r\n`);
                                        admissionRetryTimer = window.setTimeout(onClose, 1_000);
                                    } else {
                                        term.write(`\r\n\x1b[33m${t('pages.desk.admissionRetry.title')}\x1b[0m\r\n`);
                                        admissionRetryTimer = window.setTimeout(() => {
                                            admissionRetryTimer = null;
                                            if (!disposed) connectWS();
                                        }, delay);
                                    }
                                } else if (msg.signaling_type === SIGNALING_TYPE_CODE_REPLY) {
                                    const content = msg.signaling_data.content
                                    term.write(content)
                                    // Tap a bounded ring buffer of recent output for the copilot hint.
                                    if (typeof content === 'string') {
                                        recentOutputRef.current = (recentOutputRef.current + content)
                                            .slice(-COPILOT_RECENT_OUTPUT_LIMIT)
                                    }
                                } else if (msg.signaling_type === SIGNALING_TYPE_CODE_TERMINAL_STARTED) {
                                    console.log("terminal started")
                                    admissionRetry.reset()
                                    if (admissionRetryTimer !== null) {
                                        window.clearTimeout(admissionRetryTimer)
                                        admissionRetryTimer = null
                                    }
                                    terminalStarted.current = true
                                    markTerminalStarted()
                                    // Send resize again after started to be safe
                                    sendResize({ rows: term.rows, cols: term.cols })
                                } else if (msg.signaling_type === SIGNALING_TYPE_CODE_TERMINAL_CLOSED) {
                                    console.log("terminal closed")
                                    terminalStarted.current = false
                                    markTerminalClosed()
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
                        markTerminalClosed()

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
            disposed = true;
            terminalStarted.current = false
            markTerminalClosed()
            clearTimeout(connectTimer);
            if (admissionRetryTimer !== null) {
                clearTimeout(admissionRetryTimer)
                admissionRetryTimer = null
            }
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
    }, [connectionId, deviceId, command, onClose, t, markTerminalStarted, markTerminalClosed])

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
                        {t('pages.deskTerminal.switchShell')}
                    </Button>
                </div>
                {/* Manager-only completion-model picker; renders nothing against an
                    open-source signal server (or when no completion model exists),
                    leaving the completion flow unchanged. Shown only while the
                    completion assist is on, sitting under the top control bar. */}
                {completionEnabled && (
                    <div className="absolute top-12 right-4 z-10 w-56">
                        <ModelSelector
                            role="completion"
                            orgId={orgId}
                            onChange={setCompletionModelId}
                            label={t('pages.desk.modelSelector.completionLabel')}
                            className="border-input bg-[#2a2a2a] text-gray-200"
                        />
                    </div>
                )}
                <div className="flex-1 w-full p-2 overflow-hidden relative">
                    <div className="absolute inset-2 overflow-hidden" ref={terminalRef} />
                </div>
                {!isConnected && (
                    <div className="absolute top-1/2 left-1/2 transform -translate-x-1/2 -translate-y-1/2 text-white flex items-center gap-2 pointer-events-none">
                        <Loader2 className="h-6 w-6 animate-spin" />
                        <span>{t('pages.deskTerminal.connecting')}</span>
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
                        {/* Art.50(2): an AI-sourced suggestion is a novel model output, so
                            it carries the visible "AI-generated" marking (the L1 history
                            guess is local, not AI, and is not marked). */}
                        {ghost.source === 'ai' && (
                            <AiGeneratedMark provenance={ghost.provenance} className="ml-auto shrink-0 border-white/25 bg-white/10 text-gray-300" />
                        )}
                        <span className={`${ghost.source === 'ai' ? '' : 'ml-auto '}shrink-0 rounded border border-gray-600 px-1.5 py-0.5 text-[10px] text-gray-400`}>
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
                    orgId={orgId}
                />
            )}
        </div>
    )
}
