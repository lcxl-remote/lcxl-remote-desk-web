import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { DiagnosePanel } from "./diagnose-panel";
import type { DiagnoseState } from "./use-desk-diagnose";
import type { ExecEntry } from "../exec/use-confirm-exec";
import { deskErrorCodeEnum } from "@/services";

// i18n: real en-US locale; i18n.language is the tag forwarded to onStart so the
// AI answers in the UI language.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()));

const baseState: DiagnoseState = {
    phase: "idle",
    conversationId: null,
    requestId: null,
    question: "",
    status: null,
    partialSummary: "",
    result: null,
    error: null,
    errorCode: null,
    turnId: null,
    timeline: [],
    provenance: null,
    pendingExec: null,
    backgroundExecution: null,
    history: [],
};

const assistantItem = (
    text: string,
    id = "assistant-1",
    provenance: DiagnoseState["provenance"] = null,
) => ({ kind: "assistant" as const, id, text, provenance });

type TestToolActivity = Extract<
    DiagnoseState["timeline"][number],
    { kind: "tool" }
>["activity"];

const toolItem = (
    activity: Omit<TestToolActivity, "backgroundTaskId"> & {
        backgroundTaskId?: string | null;
    },
) => ({
    kind: "tool" as const,
    id: activity.callId,
    activity: { backgroundTaskId: null, ...activity },
});

function renderPanel(state: Partial<DiagnoseState>) {
    const onStart = vi.fn();
    const onReset = vi.fn();
    const onClose = vi.fn();
    render(
        <DiagnosePanel
            state={{ ...baseState, ...state }}
            onStart={onStart}
            onReset={onReset}
            onClose={onClose}
        />,
    );
    return { onStart, onReset, onClose };
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
            // The model selector is hidden in tests (no manager endpoints), so no
            // model is chosen; with no org context the org hint is omitted too.
            modelId: null,
            orgId: undefined,
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

    it("lists historical diagnoses and restores the selected session", () => {
        const onRefreshHistory = vi.fn();
        const onRestoreSession = vi.fn();
        const session = {
            sessionId: "session-1",
            conversationId: "conversation-1",
            firstQuestion: "Why is CPU usage high?",
            createdAt: "2026-07-20T08:00:00Z",
            updatedAt: "2026-07-20T08:05:00Z",
            active: false,
            messageCount: 3,
        };
        render(
            <DiagnosePanel
                state={baseState}
                onStart={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
                historySessions={[session]}
                onRefreshHistory={onRefreshHistory}
                onRestoreSession={onRestoreSession}
            />,
        );

        fireEvent.click(screen.getByText("History"));
        expect(onRefreshHistory).toHaveBeenCalledTimes(1);
        fireEvent.click(screen.getByText("Why is CPU usage high?"));
        expect(onRestoreSession).toHaveBeenCalledWith(session);
    });

    it("marks legacy historical sessions as read-only", () => {
        render(
            <DiagnosePanel
                state={{ ...baseState, phase: "done" }}
                onStart={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
                canContinue={false}
            />,
        );
        expect(
            screen.getByText(/This older conversation predates resumable history/),
        ).toBeInTheDocument();
        expect(screen.queryByText("Ask a follow-up")).not.toBeInTheDocument();
    });

    it("running: shows the localized status and streaming summary", () => {
        renderPanel({ phase: "running", status: "modeling", partialSummary: "thinking..." });
        expect(screen.getByText("Analyzing with the model...")).toBeInTheDocument();
        expect(screen.getByText("thinking...")).toBeInTheDocument();
    });

    it("running: marks the streaming answer as AI-generated on first exposure (Art.50(2))", () => {
        // The mark must appear while text is still streaming, not only once the
        // turn settles, so a dropped final frame never leaves AI text unmarked.
        renderPanel({ phase: "running", status: "modeling", partialSummary: "checking disks..." });
        expect(screen.getByText("AI-generated")).toBeInTheDocument();
    });

    it("running: shows no marking before any streaming text is exposed", () => {
        renderPanel({ phase: "running", status: "modeling", partialSummary: "" });
        expect(screen.queryByText("AI-generated")).not.toBeInTheDocument();
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
                onReset={vi.fn()}
                onClose={vi.fn()}
                isConnected={false}
            />,
        );
        expect(screen.getByText(/Connection lost/)).toBeInTheDocument();
    });

    it("done: renders the result sections", () => {
        renderPanel({
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
    });

    it("renders a background dispatch as localized status and a structured task id", () => {
        renderPanel({
            phase: "done",
            backgroundExecution: {
                executionGeneration: "generation-bg-1",
                cancelRequested: false,
            },
            history: [
                {
                    requestId: "req-bg",
                    question: "run a 30 second command",
                    result: null,
                    summary: "",
                    timeline: [
                        toolItem({
                            callId: "call-bg",
                            name: "exec_command",
                            status: "ok",
                            argumentsJson: '{"command":"Start-Sleep -Seconds 30"}',
                            output:
                                '{"status":"background_running","background_task_id":"exec_task_30"}',
                            backgroundTaskId: "exec_task_30",
                        }),
                    ],
                    phase: "done",
                    error: null,
                    errorCode: null,
                    provenance: null,
                },
            ],
        });
        expect(screen.getByText("Moved to background")).toBeInTheDocument();
        fireEvent.click(screen.getByText("exec_command"));
        expect(screen.getByText("exec_task_30")).toBeVisible();
        expect(
            screen.queryByText(/command dispatched as background task/),
        ).not.toBeInTheDocument();
    });

    it("error: shows the failure message", () => {
        renderPanel({ phase: "error", error: "evidence redaction failed" });
        expect(screen.getByText("evidence redaction failed")).toBeInTheDocument();
    });

    it("error: localizes the same-tool repeat circuit breaker by error code", () => {
        renderPanel({
            phase: "error",
            partialSummary: "CPU usage is concentrated in process 4242.",
            timeline: [
                toolItem({
                    callId: "c1",
                    name: "execute_command",
                    status: "ok",
                    argumentsJson: "{}",
                    output: "done",
                }),
            ],
            error: "the assistant stopped after repeating the same action too many times",
            errorCode: deskErrorCodeEnum.AGENT_SAME_TOOL_REPEAT_LIMIT,
        });
        expect(
            screen.getByText("CPU usage is concentrated in process 4242."),
        ).toBeInTheDocument();
        expect(screen.getByText("execute_command")).toBeInTheDocument();
        expect(
            screen.getByText(
                "The AI stopped to prevent a loop after requesting the same type of action repeatedly. Previous results were kept; you can ask it to continue.",
            ),
        ).toBeInTheDocument();
        expect(
            screen.queryByText(
                "the assistant stopped after repeating the same action too many times",
            ),
        ).not.toBeInTheDocument();
    });

    it("expands a tool call to show formatted input and output", () => {
        renderPanel({
            phase: "done",
            timeline: [
                toolItem({
                    callId: "c1",
                    name: "read_process_list",
                    status: "ok",
                    argumentsJson: '{"limit":5,"sort":"cpu_desc"}',
                    output: "pid=42 cpu=98%",
                }),
                assistantItem("The process list was collected."),
            ],
        });

        expect(screen.queryByText(/"limit": 5/)).not.toBeVisible();
        fireEvent.click(screen.getByText("read_process_list"));
        expect(screen.getByText(/"limit": 5/)).toBeVisible();
        expect(screen.getByText("pid=42 cpu=98%")).toBeVisible();
        const tool = screen.getByText("read_process_list").closest("details");
        const answer = screen.getByText("The process list was collected.");
        expect(
            tool?.compareDocumentPosition(answer) &
                Node.DOCUMENT_POSITION_FOLLOWING,
        ).toBeTruthy();
    });

    it("appends a background completion after the dispatch result and before the AI reply", () => {
        renderPanel({
            phase: "done",
            timeline: [
                toolItem({
                    callId: "c1",
                    name: "exec_command",
                    status: "ok",
                    argumentsJson: '{"command":"Start-Sleep -Seconds 30"}',
                    output:
                        '{"status":"background_running","background_task_id":"exec_task_30"}',
                    backgroundTaskId: "exec_task_30",
                }),
                {
                    kind: "background_completion",
                    id: "out1",
                    toolCallId: "c1",
                    taskId: "exec_task_30",
                    output: "exit_code=0\nstdout=finished",
                },
                assistantItem("The background command finished normally."),
            ],
        });

        const tool = screen.getByText("exec_command").closest("details");
        const completionTitle = screen.getByText("Background task finished");
        const completion = completionTitle.closest("details");
        const answer = screen.getByText("The background command finished normally.");
        expect(
            tool?.compareDocumentPosition(completionTitle) &
                Node.DOCUMENT_POSITION_FOLLOWING,
        ).toBeTruthy();
        expect(
            completion?.compareDocumentPosition(answer) &
                Node.DOCUMENT_POSITION_FOLLOWING,
        ).toBeTruthy();

        fireEvent.click(completionTitle);
        expect(screen.getAllByText(/Task ID/).some((node) => node.textContent)).toBe(true);
        expect(
            screen
                .getAllByText("exec_task_30")
                .filter((node) => (node.closest("details") as HTMLDetailsElement).open),
        ).toHaveLength(1);
        expect(screen.getByText(/stdout=finished/)).toBeVisible();
        fireEvent.click(screen.getByText("exec_command"));
        expect(
            screen
                .getAllByText("exec_task_30")
                .filter((node) => (node.closest("details") as HTMLDetailsElement).open),
        ).toHaveLength(2);
        expect(
            screen.queryByText("command dispatched as background task"),
        ).not.toBeInTheDocument();
    });

    it("done: a follow-up composer continues the conversation", () => {
        const { onStart } = renderPanel({
            phase: "done",
            timeline: [assistantItem("the host is healthy")],
        });
        const followUp = screen.getByPlaceholderText("Ask a follow-up question…");
        fireEvent.change(followUp, { target: { value: "and the disk?" } });
        fireEvent.click(screen.getByText("Send follow-up"));
        expect(onStart).toHaveBeenCalledWith("and the disk?", {
            includeScreen: false,
            locale: "en-US",
            // The model selector is hidden in tests (no manager endpoints), so no
            // model is chosen; with no org context the org hint is omitted too.
            modelId: null,
            orgId: undefined,
        });
    });

    it("error: also offers a follow-up composer", () => {
        const { onStart } = renderPanel({ phase: "error", error: "boom" });
        const followUp = screen.getByPlaceholderText("Ask a follow-up question…");
        fireEvent.change(followUp, { target: { value: "retry differently" } });
        fireEvent.click(screen.getByText("Send follow-up"));
        expect(onStart).toHaveBeenCalledWith("retry differently", {
            includeScreen: false,
            locale: "en-US",
            // The model selector is hidden in tests (no manager endpoints), so no
            // model is chosen; with no org context the org hint is omitted too.
            modelId: null,
            orgId: undefined,
        });
    });

    it("running: the live turn's own question renders immediately, not a turn late", () => {
        renderPanel({ phase: "running", status: "modeling", question: "why is the disk full?" })
        expect(screen.getByText("why is the disk full?")).toBeInTheDocument()
    })

    it("done: the answered question stays visible above its result", () => {
        renderPanel({
            phase: "done",
            question: "and the disk?",
            timeline: [assistantItem("plenty free")],
        })
        expect(screen.getByText("and the disk?")).toBeInTheDocument()
        expect(screen.getByText("plenty free")).toBeInTheDocument()
    })

    it("done: safely renders GitHub-flavored Markdown in the agent answer", () => {
        const { container } = render(
            <DiagnosePanel
                state={{
                    ...baseState,
                    phase: "done",
                    timeline: [assistantItem([
                        "## CPU report",
                        "",
                        "**Usage** is high.",
                        "",
                        "| Metric | Value |",
                        "| --- | --- |",
                        "| CPU | 99% |",
                        "",
                        "<script>alert('unsafe')</script>",
                        "",
                        "![remote pixel](https://example.invalid/pixel.png)",
                    ].join("\n"))],
                }}
                onStart={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
            />,
        )

        expect(screen.getByRole("heading", { name: "CPU report" })).toBeInTheDocument()
        expect(screen.getByText("Usage").tagName).toBe("STRONG")
        expect(screen.getByRole("table")).toBeInTheDocument()
        expect(container.querySelector("script")).toBeNull()
        expect(container.querySelector("img")).toBeNull()
        expect(screen.getByText("remote pixel")).toBeInTheDocument()
    })

    it("error: the failed question stays visible above the error", () => {
        renderPanel({ phase: "error", question: "why crash?", error: "boom" })
        expect(screen.getByText("why crash?")).toBeInTheDocument()
        expect(screen.getByText("boom")).toBeInTheDocument()
    })

    it("transcript: prior settled turns render above the live turn", () => {
        renderPanel({
            phase: "running",
            status: "modeling",
            history: [
                {
                    requestId: "req-0",
                    question: "why is cpu high?",
                    result: null,
                    summary: "",
                    timeline: [assistantItem("a runaway process")],
                    phase: "done",
                    error: null,
                    errorCode: null,
                    provenance: null,
                },
            ],
        });
        expect(screen.getByText("why is cpu high?")).toBeInTheDocument();
        expect(screen.getByText("a runaway process")).toBeInTheDocument();
    });

    it("marks a settled past AI turn in the transcript and names the model when provenance is present (Art.50(2))", () => {
        // The live turn is still running (unmarked); only the settled past AI
        // answer carries the marking, mirroring the terminal copilot transcript.
        renderPanel({
            phase: "running",
            status: "modeling",
            history: [
                {
                    requestId: "req-0",
                    question: "why is cpu high?",
                    result: null,
                    summary: "",
                    timeline: [
                        assistantItem(
                            "a runaway process",
                            "assistant-1",
                            { model_id: "gpt-4o", marking_scheme: "lcxl-ai-provenance/1" },
                        ),
                    ],
                    phase: "done",
                    error: null,
                    errorCode: null,
                    provenance: { model_id: "gpt-4o", marking_scheme: "lcxl-ai-provenance/1" },
                },
            ],
        });
        expect(screen.getByText("AI-generated")).toBeInTheDocument();
        expect(
            screen.getByTitle("Generated by AI model gpt-4o"),
        ).toBeInTheDocument();
    });

    it("marks a settled past AI turn even when its provenance is absent (fail-closed)", () => {
        // Null provenance must not downgrade the past answer to "not AI": the mark
        // is driven by the AI reply being present, only the model tooltip is lost.
        renderPanel({
            phase: "running",
            status: "modeling",
            history: [
                {
                    requestId: "req-0",
                    question: "why is cpu high?",
                    result: null,
                    summary: "",
                    timeline: [assistantItem("a runaway process")],
                    phase: "done",
                    error: null,
                    errorCode: null,
                    provenance: null,
                },
            ],
        });
        expect(screen.getByText("AI-generated")).toBeInTheDocument();
    });

    it("does not mark a failed past turn in the transcript (an error carries no AI content)", () => {
        renderPanel({
            phase: "running",
            status: "modeling",
            history: [
                {
                    requestId: "req-0",
                    question: "why is cpu high?",
                    result: null,
                    summary: "",
                    timeline: [],
                    phase: "error",
                    error: "collection failed",
                    errorCode: null,
                    provenance: null,
                },
            ],
        });
        expect(screen.getByText("collection failed")).toBeInTheDocument();
        expect(screen.queryByText("AI-generated")).not.toBeInTheDocument();
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
                    approval_timeout_ms: 120000,
                    timeout_ms: 30000,
                    risk: "low",
                    requires_confirmation: true,
                    executable: true,
                    blocked_reason: null,
                },
                execRequestId: "exec-1",
                output: null,
                error: null,
            },
        });
        fireEvent.click(screen.getByText("Approve & run"));
        expect(exec.approve).toHaveBeenCalledWith(0);
        fireEvent.click(screen.getByText("Reject"));
        expect(exec.reject).toHaveBeenCalledWith(0);
    });

    it("exec: a free-form preview shows the full shell, command, Critical risk, and blocklist warning", () => {
        renderWithExec({
            0: {
                phase: "awaiting",
                preview: {
                    exec_request_id: "exec-freeform",
                    shell: "powershell",
                    command: "Restart-Service -Name Spooler -Force",
                    cwd: null,
                    approval_timeout_ms: 120000,
                    timeout_ms: 30000,
                    risk: "critical",
                    requires_confirmation: true,
                    executable: true,
                    blocked_reason: null,
                    execution_basis: "owner_blocklist_only",
                },
                execRequestId: "exec-freeform",
                output: null,
                error: null,
            },
        });

        expect(screen.getAllByText("powershell")).toHaveLength(2);
        expect(screen.getByText("Restart-Service -Name Spooler -Force")).toBeInTheDocument();
        expect(screen.getByText("Critical risk")).toBeInTheDocument();
        expect(
            screen.getByText(
                "Free-form command: only the blocklist was checked. Review every character before approving.",
            ),
        ).toBeInTheDocument();
        expect(screen.getByText("Approve & run")).not.toHaveFocus();
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

    it("agentic: a pending free-form approval shows its Critical warning with Approve / Reject", () => {
        const onApproveExec = vi.fn();
        const onRejectExec = vi.fn();
        render(
            <DiagnosePanel
                state={{
                    ...baseState,
                    phase: "running",
                    status: "modeling",
                    pendingExec: {
                        exec_request_id: "exec-1",
                        shell: "bash",
                        command: "systemctl restart nginx",
                        cwd: null,
                        approval_timeout_ms: 45000,
                        timeout_ms: 30000,
                        risk: "critical",
                        requires_confirmation: true,
                        executable: true,
                        blocked_reason: null,
                        execution_basis: "owner_blocklist_only",
                    },
                }}
                onStart={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
                onApproveExec={onApproveExec}
                onRejectExec={onRejectExec}
            />,
        );
        expect(screen.getByText("The AI wants to run a command")).toBeInTheDocument();
        expect(screen.getByText("systemctl restart nginx")).toBeInTheDocument();
        expect(screen.getByText("Critical risk")).toBeInTheDocument();
        expect(screen.getByText(/Approval window:\s*45s/)).toBeInTheDocument();
        expect(screen.getByText(/Command runtime limit:\s*30s/)).toBeInTheDocument();
        expect(
            screen.getByText(
                "Free-form command: only the blocklist was checked. Review every character before approving.",
            ),
        ).toBeInTheDocument();
        expect(screen.getByText("Approve & run")).not.toHaveFocus();
        fireEvent.click(screen.getByText("Approve & run"));
        expect(onApproveExec).toHaveBeenCalled();
        fireEvent.click(screen.getByText("Reject"));
        expect(onRejectExec).toHaveBeenCalled();
    });

    it("close button calls onClose", () => {
        const { onClose } = renderPanel({ phase: "idle" });
        fireEvent.click(screen.getByLabelText("Close"));
        expect(onClose).toHaveBeenCalled();
    });

    it("marks the structured diagnosis result as AI-generated (Art.50(2))", () => {
        renderPanel({
            phase: "done",
            provenance: { model_id: "gpt-4o", marking_scheme: "lcxl-ai-provenance/1" },
            result: {
                summary: "Port 8080 is busy",
                confidence: "high",
                findings: [],
                commands: [],
                next_steps: [],
                missing_info: [],
                collected: [],
            },
        });
        expect(screen.getByText("AI-generated")).toBeInTheDocument();
    });

    it("marks the agentic free-text answer as AI-generated even without provenance (fail-closed)", () => {
        // No provenance on the frame: the answer is still AI content, so the mark
        // must show regardless.
        renderPanel({
            phase: "done",
            timeline: [assistantItem("the host is healthy")],
            provenance: null,
        });
        expect(screen.getByText("AI-generated")).toBeInTheDocument();
    });

    it("discloses from the first interaction that the user is talking to an AI (Art.50(1))", () => {
        // The identity disclosure is a standing element shown from the idle phase,
        // separate from and alongside the accuracy disclaimer.
        renderPanel({ phase: "idle" });
        expect(
            screen.getByText("You are interacting with an AI assistant."),
        ).toBeInTheDocument();
        expect(
            screen.getByText("AI can make mistakes. Please double-check its responses."),
        ).toBeInTheDocument();
    });

    it("caps its height to the containing desk view, not the viewport, so the footer stays inside an unfullscreened window", () => {
        // The panel is absolutely positioned inside the (shorter-than-viewport)
        // desk view, which clips overflow. A viewport-relative `85vh` cap let a
        // tall result push the footer past the container's bottom edge; the cap
        // must be relative to the container (100% minus the top/bottom gap).
        const { container } = render(
            <DiagnosePanel
                state={{ ...baseState, phase: "running", status: "modeling" }}
                onStart={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const root = container.firstElementChild as HTMLElement;
        expect(root.className).toContain("max-h-[calc(100%-2rem)]");
        expect(root.className).not.toContain("max-h-[85vh]");
    });

    it("can be resized from explicit handles and offers a jump to the latest content after the user scrolls up", () => {
        const { container } = render(
            <DiagnosePanel
                state={{ ...baseState, phase: "running", status: "modeling" }}
                onStart={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const root = container.firstElementChild as HTMLElement;
        expect(root.className).toContain("overflow-hidden");
        const widthHandle = screen.getByRole("separator", {
            name: "Resize diagnosis panel width",
        }) as HTMLDivElement;
        widthHandle.setPointerCapture = vi.fn();
        widthHandle.hasPointerCapture = vi.fn(() => false);
        fireEvent.pointerDown(widthHandle, { clientX: 380, clientY: 100, pointerId: 1 });
        fireEvent.pointerMove(widthHandle, { clientX: 260, clientY: 100, pointerId: 1 });
        expect(root).toHaveStyle({ width: "500px", height: "720px" });

        const cornerHandle = screen.getByRole("separator", {
            name: "Resize diagnosis panel width and height",
        }) as HTMLDivElement;
        cornerHandle.setPointerCapture = vi.fn();
        cornerHandle.hasPointerCapture = vi.fn(() => false);
        fireEvent.pointerDown(cornerHandle, { clientX: 260, clientY: 720, pointerId: 2 });
        fireEvent.pointerMove(cornerHandle, { clientX: 200, clientY: 620, pointerId: 2 });
        expect(root).toHaveStyle({ width: "560px", height: "620px" });

        const scrollArea = screen.getByTestId("diagnose-scroll-area");
        Object.defineProperties(scrollArea, {
            scrollHeight: { configurable: true, value: 900 },
            clientHeight: { configurable: true, value: 300 },
        });
        scrollArea.scrollTop = 200;
        fireEvent.scroll(scrollArea);

        fireEvent.click(screen.getByRole("button", { name: "Scroll to latest" }));
        expect(scrollArea.scrollTop).toBe(600);
        expect(
            screen.queryByRole("button", { name: "Scroll to latest" }),
        ).not.toBeInTheDocument();
    });

    it("can be dragged by its header without turning the close button into a drag handle", () => {
        const onClose = vi.fn();
        const { container } = render(
            <DiagnosePanel
                state={{ ...baseState, phase: "running", status: "modeling" }}
                onStart={vi.fn()}
                onReset={vi.fn()}
                onClose={onClose}
            />,
        );
        const root = container.firstElementChild as HTMLElement;
        const dragHandle = screen.getByTestId("diagnose-drag-handle");
        dragHandle.setPointerCapture = vi.fn();
        dragHandle.hasPointerCapture = vi.fn(() => false);

        fireEvent.pointerDown(dragHandle, {
            clientX: 800,
            clientY: 40,
            pointerId: 1,
        });
        fireEvent.pointerMove(dragHandle, {
            clientX: 700,
            clientY: 60,
            pointerId: 1,
        });
        fireEvent.pointerUp(dragHandle, { pointerId: 1 });
        expect(root).toHaveStyle({ transform: "translate(-100px, 16px)" });

        const close = screen.getByRole("button", { name: "Close" });
        fireEvent.pointerDown(close, {
            clientX: 700,
            clientY: 60,
            pointerId: 2,
        });
        fireEvent.pointerMove(dragHandle, {
            clientX: 600,
            clientY: 100,
            pointerId: 2,
        });
        expect(root).toHaveStyle({ transform: "translate(-100px, 16px)" });
        fireEvent.click(close);
        expect(onClose).toHaveBeenCalledTimes(1);
    });

    it("keeps the entire panel inside its remote-desktop parent while dragging", () => {
        const { container } = render(
            <DiagnosePanel
                state={{ ...baseState, phase: "running", status: "modeling" }}
                onStart={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
            />,
        );
        const root = container.firstElementChild as HTMLElement;
        const parent = root.parentElement as HTMLElement;
        vi.spyOn(parent, "getBoundingClientRect").mockReturnValue({
            left: 100,
            top: 50,
            right: 900,
            bottom: 1050,
            width: 800,
            height: 1000,
            x: 100,
            y: 50,
            toJSON: () => ({}),
        });
        vi.spyOn(root, "getBoundingClientRect").mockImplementation(() => {
            const match = root.style.transform.match(
                /translate\((-?[\d.]+)px, (-?[\d.]+)px\)/,
            );
            const offsetX = Number(match?.[1] ?? 0);
            const offsetY = Number(match?.[2] ?? 0);
            const left = 504 + offsetX;
            const top = 66 + offsetY;
            return {
                left,
                top,
                right: left + 380,
                bottom: top + 720,
                width: 380,
                height: 720,
                x: left,
                y: top,
                toJSON: () => ({}),
            };
        });

        const dragHandle = screen.getByTestId("diagnose-drag-handle");
        dragHandle.setPointerCapture = vi.fn();
        dragHandle.hasPointerCapture = vi.fn(() => false);

        fireEvent.pointerDown(dragHandle, {
            clientX: 700,
            clientY: 80,
            pointerId: 1,
        });
        fireEvent.pointerMove(dragHandle, {
            clientX: -300,
            clientY: 1080,
            pointerId: 1,
        });
        fireEvent.pointerUp(dragHandle, { pointerId: 1 });
        expect(root).toHaveStyle({ transform: "translate(-388px, 248px)" });

        fireEvent.pointerDown(dragHandle, {
            clientX: 200,
            clientY: 400,
            pointerId: 2,
        });
        fireEvent.pointerMove(dragHandle, {
            clientX: 1200,
            clientY: -600,
            pointerId: 2,
        });
        expect(root).toHaveStyle({ transform: "translate(0px, 0px)" });
    });
});
