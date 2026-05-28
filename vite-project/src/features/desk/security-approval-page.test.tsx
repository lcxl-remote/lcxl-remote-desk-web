import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { MemoryRouter, Routes, Route } from "react-router-dom";

// i18n: t() echoes the fallback the component always provides.
vi.mock("react-i18next", () => ({
    useTranslation: () => ({
        t: (_key: string, fallback?: string) => fallback ?? _key,
    }),
}));

// Mutable approval_timeout for the mocked settings hook.
const h = vi.hoisted(() => ({ approvalTimeout: null as number | null }));
vi.mock("@/services/hooks/securityController/useQuerySecuritySettings", () => ({
    useQuerySecuritySettings: () => ({
        data: { data: { approval_timeout: h.approvalTimeout } },
    }),
}));

import SecurityApprovalPage from "./security-approval-page";

type FetchCall = { url: string; init?: RequestInit };
let fetchCalls: FetchCall[] = [];

const ACK = "/api/desk/security-settings/approval/ack";
const SUBMIT = "/api/desk/security-settings/approval/submit";

function bodyOf(call: FetchCall): Record<string, unknown> {
    return JSON.parse(String(call.init?.body ?? "{}"));
}

function submitCalls(): FetchCall[] {
    return fetchCalls.filter((c) => c.url.includes("/approval/submit"));
}

/** Standard fetch mock: ack returns `{data: ackReady}`, submit returns ok=submitOk. */
function mockFetch(opts: { ackReady?: boolean; ackStatus?: number; submitOk?: boolean } = {}) {
    const { ackReady = true, ackStatus = 200, submitOk = true } = opts;
    global.fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : (input as URL).toString();
        fetchCalls.push({ url, init });
        if (url.includes("/approval/ack")) {
            const ok = ackStatus >= 200 && ackStatus < 300;
            return {
                ok,
                status: ackStatus,
                json: async () => (ok ? { data: ackReady } : null),
            } as Response;
        }
        // submit
        return {
            ok: submitOk,
            status: submitOk ? 200 : 500,
            json: async () => ({ data: true }),
        } as Response;
    }) as unknown as typeof fetch;
}

/** Ack mock whose resolution is controlled by the returned `resolve` fn. */
function mockDeferredAck() {
    let resolveAck!: (ready: boolean) => void;
    global.fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : (input as URL).toString();
        fetchCalls.push({ url, init });
        if (url.includes("/approval/ack")) {
            return new Promise<Response>((res) => {
                resolveAck = (ready: boolean) =>
                    res({ ok: true, status: 200, json: async () => ({ data: ready }) } as Response);
            });
        }
        return { ok: true, status: 200, json: async () => ({ data: true }) } as Response;
    }) as unknown as typeof fetch;
    return { resolveAck: (ready: boolean) => resolveAck(ready) };
}

function renderPage(query: string) {
    return render(
        <MemoryRouter initialEntries={[`/security-approval${query}`]}>
            <Routes>
                <Route path="/security-approval" element={<SecurityApprovalPage />} />
            </Routes>
        </MemoryRouter>,
    );
}

const QUERY =
    "?req_id=r1&from_connection_id=conn-9&i18n_key=security.permission.terminal&permission_type=Terminal";

beforeEach(() => {
    fetchCalls = [];
    h.approvalTimeout = null;
    Object.defineProperty(navigator, "sendBeacon", {
        value: vi.fn(() => true),
        configurable: true,
        writable: true,
    });
});

afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
});

describe("SecurityApprovalPage", () => {
    it("decodes query params and acks with the decoded req_id", async () => {
        mockFetch({ ackReady: true });
        renderPage(
            "?req_id=a%26b%23c%20d&from_connection_id=u%25&i18n_key=k&permission_type=Terminal",
        );
        // from_connection_id rendered decoded.
        expect(screen.getByText("u%")).toBeInTheDocument();
        await waitFor(() => {
            const ack = fetchCalls.find((c) => c.url.includes("/approval/ack"));
            expect(ack).toBeTruthy();
            expect(bodyOf(ack!).req_id).toBe("a&b#c d");
        });
    });

    it("disables buttons until ack succeeds, then enables them", async () => {
        const { resolveAck } = mockDeferredAck();
        renderPage(QUERY);
        // Before ack resolves: connecting + disabled buttons; clicks are no-ops.
        expect(screen.getByText("Connecting…")).toBeInTheDocument();
        const allow = screen.getByRole("button", { name: "Allow" });
        expect(allow).toBeDisabled();
        fireEvent.click(allow);
        expect(submitCalls()).toHaveLength(0);

        resolveAck(true);
        await waitFor(() => expect(allow).not.toBeDisabled());
    });

    it("Allow submits approved:true", async () => {
        mockFetch({ ackReady: true });
        renderPage(QUERY);
        const allow = screen.getByRole("button", { name: "Allow" });
        await waitFor(() => expect(allow).not.toBeDisabled());
        fireEvent.click(allow);
        await waitFor(() => expect(submitCalls()).toHaveLength(1));
        expect(bodyOf(submitCalls()[0])).toMatchObject({ req_id: "r1", approved: true });
    });

    it("Deny submits approved:false", async () => {
        mockFetch({ ackReady: true });
        renderPage(QUERY);
        const deny = screen.getByRole("button", { name: /Deny/ });
        await waitFor(() => expect(deny).not.toBeDisabled());
        fireEvent.click(deny);
        await waitFor(() => expect(submitCalls()).toHaveLength(1));
        expect(bodyOf(submitCalls()[0])).toMatchObject({ req_id: "r1", approved: false });
    });

    it("user closing the window sends a deny beacon", async () => {
        mockFetch({ ackReady: true });
        renderPage(QUERY);
        await waitFor(() =>
            expect(screen.getByRole("button", { name: "Allow" })).not.toBeDisabled(),
        );
        window.dispatchEvent(new Event("beforeunload"));
        expect(navigator.sendBeacon).toHaveBeenCalledTimes(1);
        const [url, blob] = (navigator.sendBeacon as ReturnType<typeof vi.fn>).mock.calls[0];
        expect(url).toBe(SUBMIT);
        const text = await (blob as Blob).text();
        expect(JSON.parse(text)).toMatchObject({ req_id: "r1", approved: false });
    });

    it("close while ack still pending also sends a deny beacon (no deadlock)", async () => {
        mockDeferredAck();
        renderPage(QUERY);
        // ack not resolved yet
        window.dispatchEvent(new Event("beforeunload"));
        expect(navigator.sendBeacon).toHaveBeenCalledTimes(1);
    });

    it("does not send a deny beacon after a successful submit (program close)", async () => {
        mockFetch({ ackReady: true, submitOk: true });
        renderPage(QUERY);
        const allow = screen.getByRole("button", { name: "Allow" });
        await waitFor(() => expect(allow).not.toBeDisabled());
        fireEvent.click(allow);
        await waitFor(() => expect(submitCalls()).toHaveLength(1));
        // The backend will destroy the window; a beforeunload then must NOT deny.
        window.dispatchEvent(new Event("beforeunload"));
        expect(navigator.sendBeacon).not.toHaveBeenCalled();
    });

    it("failed submit keeps the window open, re-enables, and shows an error", async () => {
        mockFetch({ ackReady: true, submitOk: false });
        renderPage(QUERY);
        const allow = screen.getByRole("button", { name: "Allow" });
        await waitFor(() => expect(allow).not.toBeDisabled());
        fireEvent.click(allow);
        await waitFor(() =>
            expect(screen.getByText("Failed to submit. Please try again.")).toBeInTheDocument(),
        );
        expect(allow).not.toBeDisabled();
    });

    it("ack failure (ready:false) shows not-ready and keeps buttons disabled", async () => {
        mockFetch({ ackReady: false });
        renderPage(QUERY);
        await waitFor(() =>
            expect(
                screen.getByText(/The system is not ready/),
            ).toBeInTheDocument(),
        );
        expect(screen.getByRole("button", { name: "Allow" })).toBeDisabled();
    });

    it("ack 401 shows not-ready and keeps buttons disabled", async () => {
        mockFetch({ ackStatus: 401 });
        renderPage(QUERY);
        await waitFor(() =>
            expect(screen.getByText(/The system is not ready/)).toBeInTheDocument(),
        );
        expect(screen.getByRole("button", { name: "Allow" })).toBeDisabled();
    });

    it("approval_timeout > 0 auto-denies on countdown expiry", async () => {
        h.approvalTimeout = 1;
        mockFetch({ ackReady: true });
        renderPage(QUERY);
        await waitFor(() =>
            expect(screen.getByRole("button", { name: "Allow" })).not.toBeDisabled(),
        );
        await waitFor(() => expect(submitCalls()).toHaveLength(1), { timeout: 3000 });
        expect(bodyOf(submitCalls()[0])).toMatchObject({ approved: false });
    });

    it("approval_timeout null does not auto-deny", async () => {
        h.approvalTimeout = null;
        mockFetch({ ackReady: true });
        renderPage(QUERY);
        await waitFor(() =>
            expect(screen.getByRole("button", { name: "Allow" })).not.toBeDisabled(),
        );
        // Give any (incorrect) countdown a chance to fire.
        await new Promise((r) => setTimeout(r, 200));
        expect(submitCalls()).toHaveLength(0);
    });
});
