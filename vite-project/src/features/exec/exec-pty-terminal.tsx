import { useEffect, useRef } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';

import type { ExecPtyClient } from './exec-pty-client';

/**
 * A deliberately isolated PTY surface. Unlike the long-lived terminal feature,
 * it has no command history, completion, reference store, analytics, transcript,
 * or local input-line state. xterm renders only bytes echoed by the remote PTY,
 * so password echo remains controlled by the remote terminal mode.
 */
export function ExecPtyTerminal({ client }: { client: ExecPtyClient }) {
    const containerRef = useRef<HTMLDivElement>(null);

    useEffect(() => {
        const container = containerRef.current;
        if (!container) return;
        const terminal = new Terminal({
            cursorBlink: true,
            convertEol: false,
            scrollback: 1000,
            fontSize: 12,
            fontFamily: 'Menlo, Monaco, "Courier New", monospace',
            theme: { background: '#09090b' },
        });
        const fit = new FitAddon();
        terminal.loadAddon(fit);
        terminal.open(container);
        const safeFit = () => {
            if (container.clientWidth > 0 && container.clientHeight > 0) {
                try {
                    fit.fit();
                    client.resize(terminal.rows, terminal.cols);
                } catch {
                    // A concurrent unmount can invalidate xterm dimensions.
                }
            }
        };
        safeFit();
        terminal.focus();

        const detachOutput = client.attachOutput((bytes) => terminal.write(bytes));
        const input = terminal.onData((value) => {
            client.sendInput(new TextEncoder().encode(value));
        });
        const binary = terminal.onBinary((value) => {
            const bytes = new Uint8Array(value.length);
            for (let index = 0; index < value.length; index += 1) {
                bytes[index] = value.charCodeAt(index) & 0xff;
            }
            client.sendInput(bytes);
        });
        const resize = terminal.onResize(({ rows, cols }) => client.resize(rows, cols));
        const observer = new ResizeObserver(safeFit);
        observer.observe(container);

        return () => {
            observer.disconnect();
            resize.dispose();
            binary.dispose();
            input.dispose();
            detachOutput();
            terminal.dispose();
        };
    }, [client]);

    return (
        <div
            ref={containerRef}
            data-testid="exec-pty-terminal"
            className="mt-2 h-52 min-w-0 overflow-hidden rounded border border-white/10 bg-zinc-950 p-1"
        />
    );
}
