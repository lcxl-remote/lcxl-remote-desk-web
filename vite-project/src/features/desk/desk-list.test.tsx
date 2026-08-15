import { describe, it, expect, vi, afterEach } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";

// i18n: real en-US locale so assertions match what users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()));

// Mutable connection list for the mocked query hook.
const h = vi.hoisted(() => ({
    connections: [] as unknown[],
    isLoading: false,
    isFetching: false,
    refetch: vi.fn(),
}));
vi.mock("@/services/hooks/connectionController/useListConnections", () => ({
    useListConnections: () => ({
        data: h.connections,
        isLoading: h.isLoading,
        isFetching: h.isFetching,
        refetch: h.refetch,
    }),
}));
// Desk-list platform tests do not exercise host-readiness queries. Keep that
// independently tested child out of this unit fixture so it does not require a
// QueryClientProvider unrelated to the assertions below.
vi.mock("@/features/desk/host-readiness-banners", () => ({
    HostReadinessBanners: () => null,
}));

import DeskList from "./desk-list";

function makeConnection(operationSystem: string | null) {
    return {
        connection_id: "conn-1",
        ip: "10.0.0.1",
        version_info: {
            display_name: "My Desk",
            operation_system: operationSystem,
            remote_desk_type: "Desk",
        },
    };
}

function renderList() {
    return render(
        <MemoryRouter>
            <DeskList />
        </MemoryRouter>,
    );
}

afterEach(() => {
    h.connections = [];
    h.isLoading = false;
    h.isFetching = false;
    vi.clearAllMocks();
});

describe("DeskList platform column", () => {
    it("disables and spins the manual refresh while refetching", () => {
        h.isFetching = true;
        renderList();
        const refresh = screen.getByRole("button", { name: "Refresh" });
        expect(refresh).toBeDisabled();
        expect(refresh).toHaveAttribute("aria-busy", "true");
        expect(refresh.querySelector("svg")).toHaveClass("animate-spin");
        fireEvent.click(refresh);
        expect(h.refetch).not.toHaveBeenCalled();
    });

    it("renders the reported OS for a non-Windows client (Android), not a hardcoded Windows", () => {
        // Regression: the platform cell used to be hardcoded to "Windows",
        // so an Android host was mislabelled in the list even though the
        // dashboard read version_info.operation_system correctly.
        h.connections = [makeConnection("Android")];
        renderList();
        expect(screen.getByText("Android")).toBeInTheDocument();
        expect(screen.queryByText("Windows")).not.toBeInTheDocument();
    });

    it("renders Windows when the client actually reports Windows", () => {
        h.connections = [makeConnection("Windows")];
        renderList();
        expect(screen.getByText("Windows")).toBeInTheDocument();
    });

    it("falls back to Unknown when operation_system is missing", () => {
        h.connections = [makeConnection(null)];
        renderList();
        expect(screen.getByText("Unknown")).toBeInTheDocument();
    });
});
