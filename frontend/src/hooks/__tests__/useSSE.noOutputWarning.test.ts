// @vitest-environment jsdom
//
// The run_ended "no output" warning exists for a misconfigured agent whose
// process exits cleanly without producing anything. Its trigger is
// `reason === "Completed" && !receivedContentRef.current`, so the ref must
// track ANY visible output — not just text. A tool-only turn (agent acts
// without a final sentence) or a form-only turn (AskUserQuestionWithForm)
// renders real rows in the transcript; warning there is a false positive
// that tells the user to check their profile when nothing is wrong.
//
// Driven through the real `useSSE` hook + the SSE hub's `__dispatchForTest`
// seam (same approach as `useSSE.terminalArtifact.test.ts`).

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useChatStore } from "../../stores/chatStore";
import { useSSE } from "../useSSE";
import { __dispatchForTest } from "../../lib/sseHub";

// run_ended's finalize path fires a fire-and-forget sidebar refresh and
// form_posted kicks a background transcript refresh via selectAgent — keep
// both off the network (see useSSE.terminalArtifact.test.ts).
vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return {
    ...actual,
    getAgents: async () => [],
    getAgent: async () => ({}),
    getMessages: async () => ({ messages: [], cursor: null }),
  };
});

vi.mock("../sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

const AGENT_ID = "no-output-agent";
const WARNING_FRAGMENT = "no output was received";

let mountedRoots: Array<{ root: Root; container: HTMLDivElement }> = [];

function mountHook(useHook: () => unknown): void {
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);
  function Harness() {
    useHook();
    return null;
  }
  act(() => {
    root.render(React.createElement(Harness));
  });
  mountedRoots.push({ root, container });
}

function unmountAllHooks(): void {
  act(() => {
    for (const { root } of mountedRoots) root.unmount();
  });
  for (const { container } of mountedRoots) document.body.removeChild(container);
  mountedRoots = [];
}

function inject(eventName: string, data: Record<string, unknown> = {}, threadId?: string): void {
  act(() => {
    __dispatchForTest({
      agent_id: AGENT_ID,
      run_id: "run-1",
      thread_id: threadId ?? null,
      eventName,
      // `thread_id` lives on the AgentEvent envelope, NOT inside payload
      // data — the useSSE parsePayloadData bridge merges it in. Modeling it
      // at the envelope level is what exercises that bridge.
      raw: JSON.stringify({
        agent_id: AGENT_ID,
        run_id: "run-1",
        ...(threadId ? { thread_id: threadId } : {}),
        payload: { type: eventName, data },
      }),
    });
  });
}

function warningMessages(): Array<{ content: string }> {
  return useChatStore
    .getState()
    .messages.filter((m) => m.role === "system" && m.content.includes(WARNING_FRAGMENT));
}

function runTurnWith(events: Array<[string, Record<string, unknown>]>): void {
  inject("run_started");
  for (const [name, data] of events) inject(name, data);
  inject("run_ended", { reason: "Completed" });
}

beforeEach(() => {
  useChatStore.getState().reset();
  useChatStore.setState({ selectedAgentId: AGENT_ID });
});

afterEach(() => {
  unmountAllHooks();
  vi.useRealTimers();
});

describe("run_ended no-output warning", () => {
  it("warns when a Completed run produced nothing at all", () => {
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([]);
    expect(warningMessages()).toHaveLength(1);
  });

  it("does not warn when the turn produced tool calls but no text (tool-only turn)", () => {
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([
      ["tool_call_started", { tool_name: "Bash", tool_input: { command: "ls" } }],
      ["tool_call_completed", { tool_name: "Bash", output: "ok" }],
    ]);
    expect(warningMessages()).toHaveLength(0);
  });

  it("does not warn when the turn presented a form but no text", () => {
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([
      ["form_request", { form_id: "form-1", title: "Pick one", questions: [] }],
    ]);
    expect(warningMessages()).toHaveLength(0);
  });

  it("does not warn on a normal text turn (control)", () => {
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([["text_delta", { text: "Here you go." }]]);
    expect(warningMessages()).toHaveLength(0);
  });

  it("does not warn when the whole reply arrives as a lone text_complete", () => {
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([["text_complete", { text: "Solo reply." }]]);
    expect(warningMessages()).toHaveLength(0);
  });

  it("still warns when the lone text_complete is whitespace-only", () => {
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([["text_complete", { text: "   \n  " }]]);
    expect(warningMessages()).toHaveLength(1);
  });

  it("does not warn when the turn parked on an async form (form_posted) with no text", () => {
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([
      ["form_posted", { form_id: "form-async-1", spec: { title: "Pick" } }],
    ]);
    expect(warningMessages()).toHaveLength(0);
  });

  it("does not warn when a reconnect lands in the run's quiet tail after content was seen", () => {
    // The agent_busy reset (needed for the stale-flag case) would fabricate
    // a warning here without the in-flight-store fallback: the reply text
    // arrived before the drop, only run_ended arrives after the reconnect.
    mountHook(() => useSSE(AGENT_ID));
    inject("run_started");
    inject("text_delta", { text: "Here you go." });
    inject("agent_busy"); // reconnect replay — resets the flag
    inject("run_ended", { reason: "Completed" });
    expect(warningMessages()).toHaveLength(0);
  });

  it("does not warn when only a tool_call_completed arrives after a reconnect", () => {
    // Same quiet-tail shape with a tool-only turn: the completion re-arms
    // the flag even though its tool_call_started went to the dropped
    // connection.
    mountHook(() => useSSE(AGENT_ID));
    inject("run_started");
    inject("agent_busy");
    inject("tool_call_completed", { tool_name: "Bash", tool_use_id: "t1", output: "ok" });
    inject("run_ended", { reason: "Completed" });
    expect(warningMessages()).toHaveLength(0);
  });

  it("warns after a reconnect re-arm (agent_busy) when the run then completes with nothing", () => {
    // The stale-flag bug: run 1 latches content, the connection drops, the
    // reconnect replays agent_busy for a run whose run_started we never
    // saw, and that run then exits empty. Without the reset, run 1's `true`
    // suppresses the warning — the exact misconfigured-agent case the
    // feature exists for.
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([["text_delta", { text: "First run spoke." }]]);
    inject("agent_busy");
    inject("run_ended", { reason: "Completed" });
    expect(warningMessages()).toHaveLength(1);
  });

  it("does not leak a finished run's content flag into a later run_ended (read-and-delete)", () => {
    // run_ended arrives without a matching run_started (lost on a dropped
    // connection): the verdict must come from THIS run, not the previous
    // one's leftover entry.
    mountHook(() => useSSE(AGENT_ID));
    runTurnWith([["text_delta", { text: "First run spoke." }]]);
    inject("run_ended", { reason: "Completed" });
    expect(warningMessages()).toHaveLength(1);
  });

  it("tracks content per thread: another thread's output does not mask this thread's empty completion", () => {
    // max_instances > 1: two runs on two threads of the same agent. Thread
    // B produced text; thread A (the default thread, the one on screen)
    // completed empty — the warning must still fire for A.
    mountHook(() => useSSE(AGENT_ID));
    inject("run_started"); // thread A (default)
    inject("run_started", {}, "thread-b");
    inject("text_delta", { text: "B is working." }, "thread-b");
    inject("run_ended", { reason: "Completed" }); // thread A ends empty
    expect(warningMessages()).toHaveLength(1);
  });
});
