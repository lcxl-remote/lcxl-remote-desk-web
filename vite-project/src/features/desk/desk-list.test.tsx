import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";

// i18n: t() echoes the fallback the component always provides.
vi.mock("react-i18next", () => ({
    useTranslation: () => ({
        t: (_key: string, fallback?: string) => fallback ?? _key,
    }),
}));

// Mutable connection list for the mocked query hook.
const h = vi.hoisted(() => ({
    connections: [] as unknown[],
    isLoading: false,
}));
vi.mock("@/services/hooks/connectionController/useListConnections", () => ({
    useListConnections: () => ({
        data: h.connections,
        isLoading: h.isLoading,
        refetch: vi.fn(),
    }),
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
    vi.clearAllMocks();
});

describe("DeskList platform column", () => {
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
