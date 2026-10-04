// @vitest-environment jsdom
//
// Regression guard for two bugs reported against the modal's Save flow:
// (1) opening an unedited existing agent left "Save Changes" permanently
//     enabled because the isDirty baseline diverged from how a couple of
//     fields seed their initial state (confirmed root cause: an unset
//     emoji seeded a *random* emoji on mount but isDirty compared against
//     the fixed "🤖" fallback); (2) the per-channel tabs each carried their
//     own "Save"-labeled button, reading as rivals to the one at the
//     bottom of the modal that actually persists the whole-profile PUT.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

// Mirrors the real `ProviderModelDiscoveryError` shape from ../../../lib/api
// (see CoordinatorConfigFields.test.tsx for why this needs to be a real
// class, not a plain object) — needed because an API-mode agent's Advanced
// tab (exercised by the Max Turns tests below) mounts CoordinatorConfigFields
// in API mode, which triggers a model-discovery effect on mount.
const { MockProviderModelDiscoveryError } = vi.hoisted(() => {
    class MockProviderModelDiscoveryError extends Error {
        code?: string;
        constructor(message: string, code?: string) {
            super(message);
            this.name = "ProviderModelDiscoveryError";
            this.code = code;
        }
    }
    return { MockProviderModelDiscoveryError };
});

vi.mock("../../../lib/api", () => ({
    createTelegramPairingCode: vi.fn(),
    deleteDiscordChannel: vi.fn(),
    deleteEmailChannel: vi.fn(),
    deleteSlackChannel: vi.fn(),
    deleteTelegramToken: vi.fn(),
    deleteProviderApiKey: vi.fn(),
    getAgentChannels: vi.fn().mockResolvedValue([]),
    getChannelSenders: vi.fn().mockResolvedValue({ senders: [] }),
    getComposedPrompt: vi.fn(),
    getProviderModels: vi.fn().mockResolvedValue([]),
    getProviderStatuses: vi.fn().mockResolvedValue([]),
    getSlackManifest: vi.fn(),
    getTelegramStatus: vi.fn().mockResolvedValue({ has_token: false, bot_username: null, enabled: false, linked: false }),
    setChannelSenders: vi.fn(),
    setDiscordChannelSecret: vi.fn(),
    setEmailChannelSecret: vi.fn(),
    setProviderApiKey: vi.fn().mockResolvedValue(undefined),
    setSlackChannelSecret: vi.fn(),
    setTelegramToken: vi.fn(),
    testSlackConnection: vi.fn(),
    unlinkTelegramChat: vi.fn(),
    upsertDiscordChannel: vi.fn(),
    upsertEmailChannel: vi.fn(),
    upsertSlackChannel: vi.fn(),
    ProviderModelDiscoveryError: MockProviderModelDiscoveryError,
    ApiError: class ApiError extends Error {
        status: number;
        constructor(status: number, message: string) {
            super(message);
            this.status = status;
        }
    },
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
    open: vi.fn().mockResolvedValue(null),
}));

import { AgentProfileModal } from "../AgentProfileModal";
import type { AgentProfile } from "../../../types/api";

/** A profile whose every field already matches the value the form's own
 *  useState initializers and coordinatorConfigFromProfile() would derive
 *  from it — i.e. opening this profile for edit should never read as
 *  dirty. Individual tests override just the field they're probing. */
function pristineProfile(overrides: Partial<AgentProfile> = {}): AgentProfile {
    return {
        id: "agent-1",
        name: "Assistant",
        description: "A helpful assistant",
        emoji: "🤖",
        provider: {
            type: "Cli",
            command: "echo",
            args: ["Hello from agent"],
            output_format: "Text",
            input_mode: "Arg",
            normalizer: null,
            model_arg: null,
            model_aliases: {},
            system_prompt_arg: null,
            session_arg: null,
            resume_args: [],
            session_id_fields: [],
            clear_env: false,
            no_output_timeout_ms: 30000,
        },
        model: null,
        skills: [],
        system_prompt: null,
        tools: null,
        env: {},
        max_instances: 1,
        timeout_seconds: 300,
        working_dir: null,
        home_dir: null,
        serialize: true,
        template: null,
        runner_mode: "cli",
        native_provider: "anthropic",
        delegates_to: [],
        persona: null,
        special_instructions: null,
        telegram: null,
        ...overrides,
    };
}

function findButton(container: HTMLElement, text: string) {
    return Array.from(container.querySelectorAll("button")).find((b) => b.textContent === text) as
        | HTMLButtonElement
        | undefined;
}

function clickTab(container: HTMLElement, label: string) {
    const btn = Array.from(container.querySelectorAll("nav button")).find(
        (b) => b.querySelector("span")?.textContent === label,
    ) as HTMLButtonElement;
    btn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
}

function setValue(el: HTMLInputElement | HTMLTextAreaElement, value: string) {
    const proto = el instanceof HTMLTextAreaElement ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, "value")!.set!;
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
}

describe("AgentProfileModal — isDirty baseline on pristine open", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("keeps Save Changes disabled when an existing agent's emoji is unset", async () => {
        const initial = pristineProfile({ emoji: undefined });

        await act(async () => {
            root.render(
                React.createElement(AgentProfileModal, {
                    open: true,
                    initial,
                    onClose: () => {},
                    onSubmit: vi.fn(),
                }),
            );
        });
        await act(async () => { await Promise.resolve(); });

        const saveButton = findButton(container, "Save Changes")!;
        expect(saveButton).toBeTruthy();
        expect(saveButton.disabled).toBe(true);
    });

    it("keeps Save Changes disabled when an existing agent's provider config is fully default", async () => {
        // Every provider/runtime field here already equals
        // DEFAULT_COORDINATOR_CONFIG_VALUE — the fully-defaulted case the
        // task's isDirty audit specifically calls out alongside emoji.
        const initial = pristineProfile();

        await act(async () => {
            root.render(
                React.createElement(AgentProfileModal, {
                    open: true,
                    initial,
                    onClose: () => {},
                    onSubmit: vi.fn(),
                }),
            );
        });
        await act(async () => { await Promise.resolve(); });

        const saveButton = findButton(container, "Save Changes")!;
        expect(saveButton).toBeTruthy();
        expect(saveButton.disabled).toBe(true);
    });

    it("enables Save Changes once a field is actually edited", async () => {
        const initial = pristineProfile();

        await act(async () => {
            root.render(
                React.createElement(AgentProfileModal, {
                    open: true,
                    initial,
                    onClose: () => {},
                    onSubmit: vi.fn(),
                }),
            );
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            setValue(container.querySelector("#ae-name") as HTMLInputElement, "Renamed Assistant");
        });

        const saveButton = findButton(container, "Save Changes")!;
        expect(saveButton.disabled).toBe(false);
    });
});

describe("AgentProfileModal — single Save persists deltas across tabs", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("submits edits from the Info tab and the Instructions tab in one click", async () => {
        const initial = pristineProfile();
        const onSubmit = vi.fn().mockResolvedValue(undefined);

        await act(async () => {
            root.render(
                React.createElement(AgentProfileModal, {
                    open: true,
                    initial,
                    onClose: () => {},
                    onSubmit,
                }),
            );
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            setValue(container.querySelector("#ae-name") as HTMLInputElement, "Renamed Assistant");
        });

        await act(async () => { clickTab(container, "Instructions"); });
        await act(async () => {
            setValue(container.querySelector("#ae-persona") as HTMLTextAreaElement, "A sharp, terse research partner.");
        });

        const saveButton = findButton(container, "Save Changes")!;
        expect(saveButton.disabled).toBe(false);
        await act(async () => {
            saveButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(onSubmit).toHaveBeenCalledTimes(1);
        const submitted = onSubmit.mock.calls[0][0] as AgentProfile;
        expect(submitted.name).toBe("Renamed Assistant");
        expect(submitted.persona).toBe("A sharp, terse research partner.");
    });

    it("never carries allowed_senders/allowed_users (or a channels array at all) in the profile-PUT payload", async () => {
        // Per-channel allowed_senders is server-authoritative in
        // LinkedSenderStore post-Tier-2 — the whole-profile Save must not
        // resurrect a client-supplied copy that could clobber it.
        const initial = pristineProfile();
        const onSubmit = vi.fn().mockResolvedValue(undefined);

        await act(async () => {
            root.render(
                React.createElement(AgentProfileModal, {
                    open: true,
                    initial,
                    onClose: () => {},
                    onSubmit,
                }),
            );
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => {
            setValue(container.querySelector("#ae-desc") as HTMLInputElement, "An updated description");
        });

        const saveButton = findButton(container, "Save Changes")!;
        await act(async () => {
            saveButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(onSubmit).toHaveBeenCalledTimes(1);
        const submitted = onSubmit.mock.calls[0][0] as AgentProfile;
        expect(submitted).not.toHaveProperty("channels");
        expect(submitted).not.toHaveProperty("allowed_senders");
        expect(submitted).not.toHaveProperty("allowed_users");
        const serialized = JSON.stringify(submitted);
        expect(serialized).not.toMatch(/allowed_senders/);
        expect(serialized).not.toMatch(/allowed_users/);
    });
});

describe("AgentProfileModal — server-owned fields survive an unrelated save", () => {
    // The PUT replaces the whole profile server-side and every omitted field
    // resets to its serde default, so every field the modal doesn't edit must
    // be carried through explicitly. A bare rename must not wipe plugin/skill
    // enablement, API-mode tunables, the delegate depth cap, the team link,
    // the AgentAuthor undo buffer, or the provider's resume/file-capability
    // wiring.
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("carries every non-edited field through the PUT payload", async () => {
        const initial = pristineProfile({
            serialize: false,
            minimal_prompt: true,
            enabled_plugins: { superpowers: { enabled: true } },
            enabled_launchpad_global_skills: ["deep-review"],
            enabled_launchpad_project_skills: { "proj-1": ["ship"] },
            thinking: { type: "adaptive" } as unknown as AgentProfile["thinking"],
            max_output_tokens: 8192,
            max_context_tokens: 64000,
            reasoning_effort: "high" as unknown as AgentProfile["reasoning_effort"],
            max_delegation_depth: 3,
            owning_team_id: "team-9",
            legacy_system_prompt: "the old prompt",
            provider: {
                ...pristineProfile().provider,
                session_id_fields: ["session_id"],
                file_capabilities: {
                    supported: true,
                    max_file_size_bytes: 1048576,
                    max_attachments_per_message: 3,
                    allowed_mime_types: ["image/png"],
                    image_mode: { type: "file_reference", instruction_template: "see {path}" },
                },
            },
        });
        const onSubmit = vi.fn().mockResolvedValue(undefined);

        await act(async () => {
            root.render(
                React.createElement(AgentProfileModal, {
                    open: true,
                    initial,
                    onClose: () => {},
                    onSubmit,
                }),
            );
        });
        await act(async () => { await Promise.resolve(); });

        // Dirty the form via an unrelated field — the bug's trigger is an
        // edit that has nothing to do with any of these fields.
        await act(async () => {
            setValue(container.querySelector("#ae-name") as HTMLInputElement, "Renamed Assistant");
        });

        const saveButton = findButton(container, "Save Changes")!;
        expect(saveButton.disabled).toBe(false);
        await act(async () => {
            saveButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(onSubmit).toHaveBeenCalledTimes(1);
        const submitted = onSubmit.mock.calls[0][0] as AgentProfile;
        expect(submitted.serialize).toBe(false);
        expect(submitted.minimal_prompt).toBe(true);
        expect(submitted.enabled_plugins).toEqual({ superpowers: { enabled: true } });
        expect(submitted.enabled_launchpad_global_skills).toEqual(["deep-review"]);
        expect(submitted.enabled_launchpad_project_skills).toEqual({ "proj-1": ["ship"] });
        expect(submitted.thinking).toEqual({ type: "adaptive" });
        expect(submitted.max_output_tokens).toBe(8192);
        expect(submitted.max_context_tokens).toBe(64000);
        expect(submitted.reasoning_effort).toBe("high");
        expect(submitted.max_delegation_depth).toBe(3);
        expect(submitted.owning_team_id).toBe("team-9");
        expect(submitted.legacy_system_prompt).toBe("the old prompt");
        expect(submitted.provider.session_id_fields).toEqual(["session_id"]);
        expect(submitted.provider.file_capabilities).toMatchObject({ supported: true });
    });
});

describe("AgentProfileModal — Max Turns control (native-runner turn cap)", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
    });

    afterEach(async () => {
        await act(async () => { root.unmount(); });
        document.body.removeChild(container);
    });

    it("renders on the Advanced tab for an API-mode agent, blocks Save on an invalid value, and carries a valid value through to the saved payload", async () => {
        const initial = pristineProfile({ runner_mode: "api" });
        const onSubmit = vi.fn().mockResolvedValue(undefined);

        await act(async () => {
            root.render(
                React.createElement(AgentProfileModal, {
                    open: true,
                    initial,
                    onClose: () => {},
                    onSubmit,
                }),
            );
        });
        await act(async () => { await Promise.resolve(); });

        await act(async () => { clickTab(container, "Advanced Settings"); });
        const maxTurnsInput = container.querySelector("#ae-max-turns") as HTMLInputElement;
        expect(maxTurnsInput).toBeTruthy();

        // Invalid (0): Save must be disabled outright, not silently clamp
        // or drop the value.
        await act(async () => { setValue(maxTurnsInput, "0"); });
        const saveButton = findButton(container, "Save Changes")!;
        expect(saveButton.disabled).toBe(true);

        // Valid: Save re-enables and the value round-trips into the payload
        // this button's click actually submits.
        await act(async () => { setValue(maxTurnsInput, "25"); });
        expect(saveButton.disabled).toBe(false);

        await act(async () => {
            saveButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(onSubmit).toHaveBeenCalledTimes(1);
        const submitted = onSubmit.mock.calls[0][0] as AgentProfile;
        expect(submitted.max_turns).toBe(25);
    });

    it("leaves max_turns null in the saved payload when left blank, deferring to the backend default", async () => {
        const initial = pristineProfile({ runner_mode: "api" });
        const onSubmit = vi.fn().mockResolvedValue(undefined);

        await act(async () => {
            root.render(
                React.createElement(AgentProfileModal, {
                    open: true,
                    initial,
                    onClose: () => {},
                    onSubmit,
                }),
            );
        });
        await act(async () => { await Promise.resolve(); });

        // Dirty the form via an unrelated field — Max Turns is never touched.
        await act(async () => {
            setValue(container.querySelector("#ae-name") as HTMLInputElement, "Renamed API Agent");
        });

        const saveButton = findButton(container, "Save Changes")!;
        expect(saveButton.disabled).toBe(false);
        await act(async () => {
            saveButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await act(async () => { await Promise.resolve(); });

        expect(onSubmit).toHaveBeenCalledTimes(1);
        const submitted = onSubmit.mock.calls[0][0] as AgentProfile;
        expect(submitted.max_turns).toBeNull();
    });
});
