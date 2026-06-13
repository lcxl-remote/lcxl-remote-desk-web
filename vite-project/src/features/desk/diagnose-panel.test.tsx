import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { DiagnosePanel } from "./diagnose-panel";
import type { DiagnoseState } from "./use-desk-diagnose";

// i18n: t() echoes the fallback the component always provides.
vi.mock("react-i18next", () => ({
    useTranslation: () => ({
        t: (_key: string, fallback?: string) => fallback ?? _key,
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
        expect(onStart).toHaveBeenCalledWith("why slow?", { includeScreen: false });
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

    it("close button calls onClose", () => {
        const { onClose } = renderPanel({ phase: "idle" });
        fireEvent.click(screen.getByLabelText("Close"));
        expect(onClose).toHaveBeenCalled();
    });
});
