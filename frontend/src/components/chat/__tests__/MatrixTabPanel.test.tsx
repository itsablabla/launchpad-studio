// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

vi.mock("../../../lib/api", () => ({
    getMatrixStatus: vi.fn(),
    setMatrixConnection: vi.fn(),
    deleteMatrixConnection: vi.fn(),
    createMatrixPairingCode: vi.fn(),
    unlinkMatrixRoom: vi.fn(),
    ApiError: class ApiError extends Error {
        status: number;
        constructor(status: number, message: string) {
            super(message);
            this.status = status;
        }
    },
}));

import {
    getMatrixStatus,
    setMatrixConnection,
    deleteMatrixConnection,
    createMatrixPairingCode,
    unlinkMatrixRoom,
    type MatrixStatus,
} from "../../../lib/api";
import { MatrixTabPanel } from "../AgentProfileModal";

const getMatrixStatusMock = vi.mocked(getMatrixStatus);
const setMatrixConnectionMock = vi.mocked(setMatrixConnection);
const deleteMatrixConnectionMock = vi.mocked(deleteMatrixConnection);
const createMatrixPairingCodeMock = vi.mocked(createMatrixPairingCode);
const unlinkMatrixRoomMock = vi.mocked(unlinkMatrixRoom);

function statusFixture(overrides: Partial<MatrixStatus> = {}): MatrixStatus {
    return {
        has_token: true,
        homeserver_url: "https://matrix.example.com",
        bot_user_id: "@launchpad-bot:example.com",
        enabled: true,
        connection_state: "connected",
        linked: false,
        linked_rooms: [],
        pending_pairing_code: null,
        ...overrides,
    };
}

/** Types `value` into the input with the given id, the way the Telegram
 *  panel tests do it (native setter + input event). */
async function typeInto(container: HTMLElement, id: string, value: string) {
    const input = container.querySelector(`#${id}`) as HTMLInputElement;
    expect(input, `input #${id} exists`).not.toBeNull();
    await act(async () => {
        const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
        setter.call(input, value);
        input.dispatchEvent(new Event("input", { bubbles: true }));
    });
}

async function clickButton(container: HTMLElement, label: string | ((text: string | null) => boolean)) {
    const button = Array.from(container.querySelectorAll("button")).find((b) =>
        typeof label === "string" ? b.textContent === label : label(b.textContent),
    )!;
    expect(button, "button exists").not.toBeUndefined();
    await act(async () => {
        button.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    await act(async () => { await Promise.resolve(); });
}

describe("MatrixTabPanel", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
        getMatrixStatusMock.mockReset();
        setMatrixConnectionMock.mockReset();
        deleteMatrixConnectionMock.mockReset();
        createMatrixPairingCodeMock.mockReset();
        unlinkMatrixRoomMock.mockReset();
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("shows a save-first message in create mode without calling the status endpoint", async () => {
        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "new-agent", isCreating: true }));
        });
        expect(container.textContent).toContain("Save the agent first");
        expect(getMatrixStatusMock).not.toHaveBeenCalled();
    });

    it("renders the connection form (homeserver + username/password) when not configured", async () => {
        getMatrixStatusMock.mockResolvedValue(statusFixture({ has_token: false, homeserver_url: null, bot_user_id: null, enabled: false, connection_state: "disconnected" }));

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(getMatrixStatusMock).toHaveBeenCalledWith("agent-1");
        expect(container.querySelector("#mx-homeserver")).not.toBeNull();
        expect(container.querySelector("#mx-username")).not.toBeNull();
        expect(container.querySelector("#mx-password")).not.toBeNull();
        expect(container.querySelector("#mx-token")).toBeNull();
        expect(container.textContent).not.toContain("Connected");
    });

    it("renders the bot user id, homeserver, Enabled label, and the connection badge when configured", async () => {
        getMatrixStatusMock.mockResolvedValue(statusFixture());

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("@launchpad-bot:example.com");
        expect(container.textContent).toContain("https://matrix.example.com");
        expect(container.textContent).toContain("Enabled");
        // Unlike Telegram's tab, the badge comes from the Matrix status
        // endpoint itself — no GET …/channels side fetch.
        expect(container.textContent).toContain("Connected");
        expect(container.querySelector("#mx-homeserver")).toBeNull();
    });

    it("renders a non-alarming badge when another process holds the lease", async () => {
        getMatrixStatusMock.mockResolvedValue(statusFixture({ connection_state: "not-holding-lease" }));

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("Held by another process");
    });

    it("connects via username/password, re-reads status, and never re-renders the password", async () => {
        getMatrixStatusMock
            .mockResolvedValueOnce(statusFixture({ has_token: false, homeserver_url: null, bot_user_id: null, enabled: false, connection_state: "disconnected" }))
            .mockResolvedValueOnce(statusFixture());
        setMatrixConnectionMock.mockResolvedValue({ user_id: "@launchpad-bot:example.com" });

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await typeInto(container, "mx-homeserver", "https://matrix.example.com");
        await typeInto(container, "mx-username", "launchpad-bot");
        await typeInto(container, "mx-password", "the-bot-password");
        await clickButton(container, "Connect");

        expect(setMatrixConnectionMock).toHaveBeenCalledWith("agent-1", {
            homeserver_url: "https://matrix.example.com",
            username: "launchpad-bot",
            password: "the-bot-password",
        });
        // Status re-read after connecting; the form is gone and the
        // password is nowhere in the rendered output.
        expect(getMatrixStatusMock).toHaveBeenCalledTimes(2);
        expect(container.textContent).toContain("@launchpad-bot:example.com");
        expect(container.textContent).not.toContain("the-bot-password");
        expect(container.querySelector("#mx-password")).toBeNull();
    });

    it("connects via a raw access token when that auth mode is selected", async () => {
        getMatrixStatusMock
            .mockResolvedValueOnce(statusFixture({ has_token: false, homeserver_url: null, bot_user_id: null, enabled: false, connection_state: "disconnected" }))
            .mockResolvedValueOnce(statusFixture());
        setMatrixConnectionMock.mockResolvedValue({ user_id: "@launchpad-bot:example.com" });

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await clickButton(container, "Access token");
        expect(container.querySelector("#mx-token")).not.toBeNull();
        expect(container.querySelector("#mx-username")).toBeNull();

        await typeInto(container, "mx-homeserver", "https://matrix.example.com");
        await typeInto(container, "mx-token", "syt_SECRET");
        await clickButton(container, "Connect");

        expect(setMatrixConnectionMock).toHaveBeenCalledWith("agent-1", {
            homeserver_url: "https://matrix.example.com",
            access_token: "syt_SECRET",
        });
        expect(container.textContent).not.toContain("syt_SECRET");
    });

    it("shows the inline error message on a rejected connection attempt", async () => {
        getMatrixStatusMock.mockResolvedValue(statusFixture({ has_token: false, homeserver_url: null, bot_user_id: null, enabled: false, connection_state: "disconnected" }));
        setMatrixConnectionMock.mockRejectedValue(new Error("invalid username or password"));

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await typeInto(container, "mx-homeserver", "https://matrix.example.com");
        await typeInto(container, "mx-username", "launchpad-bot");
        await typeInto(container, "mx-password", "wrong");
        await clickButton(container, "Connect");

        expect(container.textContent).toContain("invalid username or password");
        // Still not configured — the form remains for another attempt.
        expect(container.querySelector("#mx-username")).not.toBeNull();
    });

    it("disconnects and returns to the not-configured state", async () => {
        getMatrixStatusMock
            .mockResolvedValueOnce(statusFixture())
            .mockResolvedValueOnce(statusFixture({ has_token: false, homeserver_url: "https://matrix.example.com", bot_user_id: null, enabled: false, connection_state: "disconnected" }));
        deleteMatrixConnectionMock.mockResolvedValue(undefined);

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await clickButton(container, (t) => t?.includes("Disconnect") ?? false);

        expect(deleteMatrixConnectionMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).not.toContain("@launchpad-bot:example.com");
        expect(container.querySelector("#mx-homeserver")).not.toBeNull();
    });

    it("generates a pairing code and renders it with the !agent pair instruction", async () => {
        getMatrixStatusMock
            .mockResolvedValueOnce(statusFixture())
            .mockResolvedValueOnce(statusFixture({ pending_pairing_code: { code: "MXC123", expires_at_unix: Math.floor(Date.now() / 1000) + 600 } }));
        createMatrixPairingCodeMock.mockResolvedValue({ code: "MXC123", expires_at_unix: Math.floor(Date.now() / 1000) + 600 });

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        await clickButton(container, "Generate pairing code");

        expect(createMatrixPairingCodeMock).toHaveBeenCalledWith("agent-1");
        expect(container.textContent).toContain("MXC123");
        expect(container.textContent).toContain("send !agent pair MXC123 to the bot to link this conversation.");
    });

    it("renders linked room ids and unlinks one, updating the list from the response", async () => {
        getMatrixStatusMock.mockResolvedValue(statusFixture({
            linked: true,
            linked_rooms: ["!alpha:example.com", "!beta:example.com"],
        }));
        unlinkMatrixRoomMock.mockResolvedValue({ linked_rooms: ["!beta:example.com"] });

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("!alpha:example.com");
        expect(container.textContent).toContain("!beta:example.com");

        const unlinkButtons = Array.from(container.querySelectorAll("button")).filter((b) => b.textContent === "Unlink");
        expect(unlinkButtons).toHaveLength(2);
        await act(async () => {
            unlinkButtons[0].dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(unlinkMatrixRoomMock).toHaveBeenCalledWith("agent-1", "!alpha:example.com");
        expect(container.textContent).not.toContain("!alpha:example.com");
        expect(container.textContent).toContain("!beta:example.com");
    });

    it("shows the no-rooms-linked hint and reject-all note when the allow-list is empty", async () => {
        getMatrixStatusMock.mockResolvedValue(statusFixture());

        await act(async () => {
            root.render(React.createElement(MatrixTabPanel, { agentId: "agent-1", isCreating: false }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(container.textContent).toContain("No rooms linked yet");
        expect(container.textContent).toContain("the bot ignores all incoming messages");
    });
});
