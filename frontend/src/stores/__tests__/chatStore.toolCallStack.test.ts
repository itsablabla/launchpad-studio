/**
 * Pins the tool-transcript behavior: a classic (native tool-calling, no
 * `action_id`) chip moves OUT of `activeToolCalls` and INTO the turn's
 * persistent `completedToolCalls` the moment its `tool_call_completed`
 * fires, carrying output/isError/elapsedMs. The finished call keeps
 * rendering as a collapsed `ToolCallGroup` row in the same bubble space the
 * chip occupied, so the bubble doesn't shrink-then-regrow (the original
 * "jumpy tool indicator" concern) while the transcript survives the turn —
 * and gets stamped onto the finalized message's `metadata.tool_calls`.
 *
 * `action_id`-keyed chips (tool_use/agent_action) are untouched by any of
 * this — see chatStore.toolUse.test.ts for their coexistence coverage.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

const mockSendMessage = vi.fn();
const mockGetAgents = vi.fn();

vi.mock("../../lib/api", () => ({
  getAgents: (...args: unknown[]) => mockGetAgents(...args),
  getAgent: vi.fn().mockResolvedValue(null),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  listThreads: vi.fn().mockResolvedValue([]),
  sendMessage: (...args: unknown[]) => mockSendMessage(...args),
}));

import { useChatStore } from "../chatStore";

const AGENT_ID = "agent-tool-call-stack-test";

function store() {
  return useChatStore.getState();
}

function calls() {
  return useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.activeToolCalls ?? [];
}

function completed() {
  return useChatStore.getState().inFlightByAgent.get(AGENT_ID)?.completedToolCalls ?? [];
}

beforeEach(() => {
  useChatStore.getState().reset();
  vi.clearAllMocks();
  mockGetAgents.mockResolvedValue([]);
  mockSendMessage.mockResolvedValue({ message_id: "msg-1", status: "queued" });
  useChatStore.getState().ensureInFlight(AGENT_ID);
});

describe("markInFlightToolCallDone moves the chip into the tool transcript", () => {
  it("moves the oldest not-done classic chip into completedToolCalls with its completion detail", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", input: { file_path: "/a.ts" } });
    store().markInFlightToolCallDone(AGENT_ID, { output: "file body", isError: false });

    expect(calls()).toHaveLength(0);
    expect(completed()).toHaveLength(1);
    expect(completed()[0].tool).toBe("Read");
    expect(completed()[0].input).toEqual({ file_path: "/a.ts" });
    expect(completed()[0].output).toBe("file body");
    expect(completed()[0].isError).toBe(false);
    expect(typeof completed()[0].elapsedMs).toBe("number");
  });

  it("accumulates sequential calls in order, leaving only the running one chipped", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read" });
    store().markInFlightToolCallDone(AGENT_ID);
    store().addInFlightToolCall(AGENT_ID, { tool: "Grep" });
    store().markInFlightToolCallDone(AGENT_ID);
    store().addInFlightToolCall(AGENT_ID, { tool: "Edit" });

    expect(calls().map((tc) => tc.tool)).toEqual(["Edit"]);
    expect(completed().map((tc) => tc.tool)).toEqual(["Read", "Grep"]);
  });

  it("is a no-op when there is no not-done classic chip to mark", () => {
    store().markInFlightToolCallDone(AGENT_ID);
    expect(calls()).toHaveLength(0);
    expect(completed()).toHaveLength(0);
  });

  it("records failures with isError and the error output", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "false" } });
    store().markInFlightToolCallDone(AGENT_ID, { output: "exit 1", isError: true });

    expect(completed()[0].isError).toBe(true);
    expect(completed()[0].output).toBe("exit 1");
  });
});

describe("classic tool-call stacking cap (5)", () => {
  it("salvages an evicted chip into the transcript instead of dropping it", () => {
    // Five still-running classic chips (none marked done), then a 6th —
    // the cap must evict the oldest, and the evicted call must survive as a
    // transcript row rather than vanish.
    for (let i = 0; i < 5; i++) {
      store().addInFlightToolCall(AGENT_ID, { tool: `Tool${i}` });
    }
    expect(calls()).toHaveLength(5);

    store().addInFlightToolCall(AGENT_ID, { tool: "Tool5" });

    expect(calls().map((tc) => tc.tool)).toEqual(["Tool1", "Tool2", "Tool3", "Tool4", "Tool5"]);
    expect(completed().map((tc) => tc.tool)).toEqual(["Tool0"]);
  });

  it("does not count action_id-keyed chips against the classic cap", () => {
    for (let i = 0; i < 5; i++) {
      store().addInFlightToolCall(AGENT_ID, { tool: `Tool${i}` });
    }
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    store().addInFlightToolUse(AGENT_ID, "tu-2", "RecallHistory");

    // 5 classic (at cap) + 2 action_id-keyed — cap only applies to classic.
    expect(calls()).toHaveLength(7);
  });
});

describe("repeated same-tool call after completion", () => {
  it("gives a second Read call its own fresh chip — the finished one lives in the transcript", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", input: { file_path: "/a.ts" } });
    store().markInFlightToolCallDone(AGENT_ID);

    store().addInFlightToolCall(AGENT_ID, { tool: "Read", label: "Reading b.ts" });

    expect(calls()).toHaveLength(1);
    expect(calls()[0].label).toBe("Reading b.ts");
    expect(completed()).toHaveLength(1);
    expect(completed()[0].input).toEqual({ file_path: "/a.ts" });
  });
});

describe("completedToolCalls survives mid-turn entry rebuilds", () => {
  // Regression: appendInFlightDelta / thinking events rebuild the in-flight
  // entry field-by-field — a rebuild that forgets `completedToolCalls` wipes
  // the tool transcript the moment reply text starts streaming (observed
  // live: the ToolCallGroup row vanished exactly at the first text_delta,
  // so finalize had nothing left to stamp).
  it("text_delta after a completed tool call keeps the transcript", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "echo hi" } });
    store().markInFlightToolCallDone(AGENT_ID, { output: "hi\n" });
    expect(completed()).toHaveLength(1);

    store().appendInFlightDelta(AGENT_ID, "Done — ");

    expect(completed()).toHaveLength(1);
    expect(completed()[0].tool).toBe("Execute");
  });

  it("thinking events keep the transcript too", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read" });
    store().markInFlightToolCallDone(AGENT_ID);
    store().startInFlightThinking(AGENT_ID);
    store().appendInFlightThinkingDelta(AGENT_ID, "hmm");
    expect(completed()).toHaveLength(1);
  });

  it("finalize stamps the transcript onto the message metadata after text streamed", () => {
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "echo hi" } });
    store().markInFlightToolCallDone(AGENT_ID, { output: "hi\n" });
    store().appendInFlightDelta(AGENT_ID, "All done.");

    store().finalizeInFlightText(AGENT_ID, "All done.");

    const last = store().messages[store().messages.length - 1];
    const stamped = (last.metadata as Record<string, unknown> | undefined)?.tool_calls as unknown[];
    expect(Array.isArray(stamped)).toBe(true);
    expect(stamped).toHaveLength(1);
    expect((stamped[0] as { tool: string }).tool).toBe("Execute");
  });
});

describe("id-matched completion (parallel tool calls)", () => {
  it("joins an out-of-order completion to the right chip by tool_use_id", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "sleep 9" }, toolUseId: "call-slow" });
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "echo fast" }, toolUseId: "call-fast" });

    // The fast call finishes FIRST — without id matching, FIFO would attach
    // its output to the slow call's row.
    store().markInFlightToolCallDone(AGENT_ID, { output: "fast\n", toolUseId: "call-fast" });

    expect(calls().map((tc) => tc.tool_use_id)).toEqual(["call-slow"]);
    expect(completed()).toHaveLength(1);
    expect(completed()[0]).toMatchObject({ id: "call-fast", output: "fast\n" });
    expect(completed()[0].input).toEqual({ command: "echo fast" });
  });

  it("falls back to FIFO when the completion carries no id", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read" });
    store().addInFlightToolCall(AGENT_ID, { tool: "Grep" });
    store().markInFlightToolCallDone(AGENT_ID, { output: "x" });
    expect(completed()[0].tool).toBe("Read");
  });

  it("a repeated tool_call_started for the same id updates in place instead of double-chipping", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", toolUseId: "c1" });
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "ls" }, toolUseId: "c1" });
    expect(calls()).toHaveLength(1);
    expect(calls()[0].input).toEqual({ command: "ls" });
  });

  it("the input-fill merge never merges two calls the provider gave different ids", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", toolUseId: "c1" });
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "ls" }, toolUseId: "c2" });
    expect(calls()).toHaveLength(2);
  });
});

describe("new composer turn resets a dead turn's salvaged transcript", () => {
  it("sendMessage clears completedToolCalls lingering in the teardown window", async () => {
    // Simulate a turn that ended badly: chips salvaged, entry awaiting teardown.
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "boom" } });
    store().clearInFlightToolCalls(AGENT_ID);
    expect(completed()).toHaveLength(1);

    useChatStore.setState({ selectedAgentId: AGENT_ID });
    await store().sendMessage("next question");

    // The new turn must not inherit (and later re-stamp) the dead turn's rows.
    expect(completed()).toHaveLength(0);
    expect(store().inFlightByAgent.get(AGENT_ID)?.isTyping).toBe(true);
  });

  it("sendMessage leaves a clean entry alone", async () => {
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    await store().sendMessage("hello");
    expect(completed()).toHaveLength(0);
    expect(store().inFlightByAgent.get(AGENT_ID)?.isTyping).toBe(true);
  });
});

describe("clearInFlightToolCalls salvages unfinished classic chips", () => {
  it("moves still-running classic chips into the transcript on run_ended", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read" });
    store().markInFlightToolCallDone(AGENT_ID, { output: "done" });
    store().addInFlightToolCall(AGENT_ID, { tool: "Bash", input: { command: "make" } });

    store().clearInFlightToolCalls(AGENT_ID);

    expect(calls()).toHaveLength(0);
    expect(completed().map((tc) => tc.tool)).toEqual(["Read", "Bash"]);
    expect(completed()[1].output).toBeUndefined();
  });
});

describe("flushClassicToolCalls (first text delta)", () => {
  it("salvages classic chips into the transcript and keeps action-keyed chips", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "make" }, toolUseId: "c1" });
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");

    store().flushClassicToolCalls(AGENT_ID);

    // Classic chip moved to the transcript (no output yet — still running)…
    expect(calls().map((tc) => tc.action_id)).toEqual(["tu-1"]);
    expect(completed()).toHaveLength(1);
    expect(completed()[0].id).toBe("c1");
    expect(completed()[0].output).toBeUndefined();
  });

  it("is a no-op when only action-keyed chips exist", () => {
    store().addInFlightToolUse(AGENT_ID, "tu-1", "DateTime");
    const before = store().inFlightByAgent.get(AGENT_ID);
    store().flushClassicToolCalls(AGENT_ID);
    expect(store().inFlightByAgent.get(AGENT_ID)).toBe(before);
  });
});

describe("completion after salvage patches the transcript record by id", () => {
  it("attaches output to the already-salvaged record instead of another chip", () => {
    // Parallel calls; call-b is still running when text starts streaming and
    // the text-flush salvages it. Its completion must find THAT record.
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "sleep 9" }, toolUseId: "call-slow" });
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "echo b" }, toolUseId: "call-b" });
    store().flushClassicToolCalls(AGENT_ID);
    expect(completed()).toHaveLength(2);

    store().markInFlightToolCallDone(AGENT_ID, { output: "b\n", toolUseId: "call-b" });

    expect(completed()[1].id).toBe("call-b");
    expect(completed()[1].output).toBe("b\n");
    // The other record is untouched — no FIFO misattribution.
    expect(completed()[0].output).toBeUndefined();
  });

  it("still falls back to FIFO when no id is available anywhere", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", input: { file_path: "/a.ts" } });
    store().markInFlightToolCallDone(AGENT_ID, { output: "body" });
    expect(completed()[0].output).toBe("body");
  });
});

describe("skip-listed tools never enter the transcript", () => {
  it("AskUserQuestionWithForm completes without a ToolCallGroup row", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "mcp__launchpad__AskUserQuestionWithForm", toolUseId: "f1" });
    store().markInFlightToolCallDone(AGENT_ID, { output: "{}", toolUseId: "f1" });
    expect(calls()).toHaveLength(0);
    expect(completed()).toHaveLength(0);
  });

  it("salvage paths exclude skip-listed tools", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "ArtifactWrite" });
    store().addInFlightToolCall(AGENT_ID, { tool: "Read", input: { file_path: "/a.ts" } });
    store().clearInFlightToolCalls(AGENT_ID);
    expect(completed().map((tc) => tc.tool)).toEqual(["Read"]);
  });

  it("the finalize stamp excludes skip-listed tools", () => {
    useChatStore.setState({ selectedAgentId: AGENT_ID });
    store().addInFlightToolCall(AGENT_ID, { tool: "ArtifactWrite", toolUseId: "aw-1" });
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "echo hi" }, toolUseId: "ex-1" });
    store().finalizeInFlightText(AGENT_ID, "done");

    const last = store().messages[store().messages.length - 1];
    const stamped = (last.metadata as Record<string, unknown> | undefined)?.tool_calls as { tool: string }[];
    expect(stamped.map((r) => r.tool)).toEqual(["Execute"]);
  });
});

describe("output storage cap", () => {
  it("truncates a huge tool output on the transcript record", () => {
    store().addInFlightToolCall(AGENT_ID, { tool: "Execute", input: { command: "make test" }, toolUseId: "big" });
    const huge = "x".repeat(300_000);
    store().markInFlightToolCallDone(AGENT_ID, { output: huge, toolUseId: "big" });
    expect(completed()[0].output!.length).toBeLessThan(210_000);
    expect(completed()[0].output).toContain("(truncated)");
  });
});
