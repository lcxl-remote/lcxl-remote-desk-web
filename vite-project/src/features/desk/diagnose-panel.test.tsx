import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { DiagnosePanel } from "./diagnose-panel";
import type { DiagnoseState } from "./use-desk-diagnose";
import type { ExecEntry } from "./use-desk-exec";

// i18n: t() echoes the fallback the component always provides; i18n.language is
// the tag forwarded to onStart so the AI answers in the UI language.
vi.mock("react-i18next", () => ({
    useTranslation: () => ({
        t: (_key: string, fallback?: string) => fallback ?? _key,
        i18n: { language: "en-US" },
    }),
}));

const baseState: DiagnoseState = {
    phase: "idle",
    requestId: null,
    status: null,
    partialSummary: "",
    result: null,
    error: null,
};

function renderPanel(state: Partial<DiagnoseState>) {
    const onStart = vi.fn();
    const onHandoff = vi.fn();
    const onReset = vi.fn();
    const onClose = vi.fn();
    render(
        <DiagnosePanel
            state={{ ...baseState, ...state }}
            onStart={onStart}
            onHandoff={onHandoff}
            onReset={onReset}
            onClose={onClose}
        />,
    );
    return { onStart, onHandoff, onReset, onClose };
}

describe("DiagnosePanel", () => {
    it("idle: typing a question and submitting calls onStart", () => {
        const { onStart } = renderPanel({ phase: "idle" });
        const textarea = screen.getByPlaceholderText(
            "e.g. The app is slow and unresponsive",
        );
        fireEvent.change(textarea, { target: { value: "why slow?" } });
        fireEvent.click(screen.getByText("Start diagnosis"));
        expect(onStart).toHaveBeenCalledWith("why slow?", {
            includeScreen: false,
            locale: "en-US",
        });
    });

    it("idle: a preset fills the question box", () => {
        renderPanel({ phase: "idle" });
        fireEvent.click(screen.getByText("Why is CPU usage so high?"));
        const textarea = screen.getByPlaceholderText(
            "e.g. The app is slow and unresponsive",
        ) as HTMLTextAreaElement;
        expect(textarea.value).toBe("Why is CPU usage so high?");
    });

    it("running: shows the localized status and streaming summary", () => {
        renderPanel({ phase: "running", status: "modeling", partialSummary: "thinking..." });
        expect(screen.getByText("Analyzing with the model...")).toBeInTheDocument();
        expect(screen.getByText("thinking...")).toBeInTheDocument();
    });

    it("running: New diagnosis is available so the user can start over without refreshing", () => {
        const { onReset } = renderPanel({ phase: "running", status: "modeling" });
        fireEvent.click(screen.getByText("New diagnosis"));
        expect(onReset).toHaveBeenCalled();
    });

    it("running + disconnected: surfaces a connection-lost hint", () => {
        render(
            <DiagnosePanel
                state={{ ...baseState, phase: "running" }}
                onStart={vi.fn()}
                onHandoff={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
                isConnected={false}
            />,
        );
        expect(screen.getByText(/Connection lost/)).toBeInTheDocument();
    });

    it("done: renders the result sections and handoff calls onHandoff", () => {
        const { onHandoff } = renderPanel({
            phase: "done",
            result: {
                summary: "Port 8080 is busy",
                confidence: "high",
                findings: [
                    { title: "Port conflict", evidence_refs: ["network.ports[3]"], explanation: "old-api holds it" },
                ],
                commands: [
                    {
                        shell: "powershell",
                        command: "Get-NetTCPConnection -LocalPort 8080",
                        purpose: "Confirm owner",
                        risk: "low",
                        requires_confirmation: false,
                    },
                ],
                next_steps: ["Stop the conflicting service"],
                missing_info: [],
                collected: ["network.ports", "process.list"],
            },
        });

        expect(screen.getByText("Port 8080 is busy")).toBeInTheDocument();
        expect(screen.getByText("Port conflict")).toBeInTheDocument();
        expect(screen.getByText("Get-NetTCPConnection -LocalPort 8080")).toBeInTheDocument();
        expect(screen.getByText("Stop the conflicting service")).toBeInTheDocument();
        expect(screen.getByText("network.ports")).toBeInTheDocument();

        fireEvent.click(screen.getByText("Hand off to human"));
        expect(onHandoff).toHaveBeenCalled();
    });

    it("error: shows the failure message", () => {
        renderPanel({ phase: "error", error: "evidence redaction failed" });
        expect(screen.getByText("evidence redaction failed")).toBeInTheDocument();
    });

    const doneWithCommand: Partial<DiagnoseState> = {
        phase: "done",
        result: {
            summary: "s",
            confidence: "high",
            findings: [],
            commands: [
                {
                    shell: "powershell",
                    command: "Get-Service -Name Spooler",
                    purpose: "check",
                    risk: "low",
                    requires_confirmation: true,
                },
            ],
            next_steps: [],
            missing_info: [],
            collected: [],
        },
    };

    function renderWithExec(entries: Record<number, ExecEntry>) {
        const exec = {
            entries,
            requestPreview: vi.fn(),
            approve: vi.fn(),
            reject: vi.fn(),
            dismiss: vi.fn(),
        };
        render(
            <DiagnosePanel
                state={{ ...baseState, ...doneWithCommand }}
                onStart={vi.fn()}
                onHandoff={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
                exec={exec}
            />,
        );
        return exec;
    }

    it("exec: Execute requests a preview for the command", () => {
        const exec = renderWithExec({});
        fireEvent.click(screen.getByText("Execute"));
        expect(exec.requestPreview).toHaveBeenCalledWith(
            0,
            expect.objectContaining({ command: "Get-Service -Name Spooler" }),
        );
    });

    it("exec: an awaiting preview shows confirm with Approve / Reject", () => {
        const exec = renderWithExec({
            0: {
                phase: "awaiting",
                preview: {
                    exec_request_id: "exec-1",
                    shell: "powershell",
                    command: "Get-Service -Name Spooler",
                    cwd: null,
                    timeout_ms: 30000,
                    risk: "low",
                    impact: "Read the status of a Windows service",
                    policy_note: "matched template get_service_named",
                    requires_confirmation: true,
                    executable: true,
                    blocked_reason: null,
                },
                execRequestId: "exec-1",
                output: null,
                error: null,
            },
        });
        expect(screen.getByText("Read the status of a Windows service")).toBeInTheDocument();
        fireEvent.click(screen.getByText("Approve & run"));
        expect(exec.approve).toHaveBeenCalledWith(0);
        fireEvent.click(screen.getByText("Reject"));
        expect(exec.reject).toHaveBeenCalledWith(0);
    });

    it("exec: a done result shows the exit code and stdout", () => {
        renderWithExec({
            0: {
                phase: "done",
                preview: null,
                execRequestId: "exec-1",
                output: {
                    exit_code: 0,
                    stdout: "Running",
                    stderr: "",
                    stdout_truncated: false,
                    stderr_truncated: false,
                    duration_ms: 12,
                    redactions: [],
                },
                error: null,
            },
        });
        expect(screen.getByText("Running")).toBeInTheDocument();
        expect(screen.getByText(/Exit\s*0/)).toBeInTheDocument();
    });

    it("exec: a blocked preview shows the reason and no Approve", () => {
        renderWithExec({
            0: {
                phase: "error",
                preview: null,
                execRequestId: null,
                output: null,
                error: "Blocked: matches a prohibited pattern (download-and-execute)",
            },
        });
        expect(
            screen.getByText("Blocked: matches a prohibited pattern (download-and-execute)"),
        ).toBeInTheDocument();
        expect(screen.queryByText("Approve & run")).not.toBeInTheDocument();
    });

    it("close button calls onClose", () => {
        const { onClose } = renderPanel({ phase: "idle" });
        fireEvent.click(screen.getByLabelText("Close"));
        expect(onClose).toHaveBeenCalled();
    });
});
