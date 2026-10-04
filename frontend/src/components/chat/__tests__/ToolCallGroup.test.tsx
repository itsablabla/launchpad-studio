// @vitest-environment jsdom
/**
 * Pins `extractToolCallsByTurn` (persisted tool_use/tool_result pairing for
 * the droid-TUI-style tool transcript) and the `toolCalls` attachment in
 * `buildMessageItems`, plus a render smoke test for `ToolCallGroup`.
 */
import { describe, it, expect } from "vitest";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { extractToolCallsByTurn, buildMessageItems } from "../MessageList";
import { ToolCallGroup } from "../ToolCallGroup";
import type { TranscriptEntry } from "../../../types/api";

const AGENT = "agent-x";
const TURN = "turn-1";

function toolUse(id: string, tool: string, ts: string, input: Record<string, unknown> = {}, turnId = TURN): TranscriptEntry {
  return {
    ts,
    role: { agent: AGENT },
    content: "",
    event_type: "tool_use",
    metadata: { turn_id: turnId, tool_use_id: id, tool_name: tool, input },
  };
}

function toolResult(id: string, ts: string, output: string, isError = false, turnId = TURN): TranscriptEntry {
  return {
    ts,
    role: "tool",
    content: "",
    event_type: "tool_result",
    metadata: { turn_id: turnId, tool_use_id: id, output, is_error: isError },
  };
}

function response(text: string, ts: string, turnId = TURN): TranscriptEntry {
  return { ts, role: { agent: AGENT }, content: text, event_type: "response", metadata: { turn_id: turnId } };
}

describe("extractToolCallsByTurn", () => {
  it("pairs tool_use with its tool_result by tool_use_id, grouped by turn", () => {
    const messages: TranscriptEntry[] = [
      toolUse("c1", "Execute", "2026-10-02T10:00:00Z", { command: "ls" }),
      toolResult("c1", "2026-10-02T10:00:02Z", "file.ts\n"),
      response("Done.", "2026-10-02T10:00:03Z"),
    ];
    const byTurn = extractToolCallsByTurn(messages);
    const calls = byTurn.get(TURN);
    expect(calls).toHaveLength(1);
    expect(calls![0]).toMatchObject({ id: "c1", tool: "Execute", output: "file.ts\n", isError: false });
    expect(calls![0].input).toEqual({ command: "ls" });
    expect(calls![0].elapsedMs).toBe(2000);
  });

  it("skips tools with their own richer rendering (forms, ArtifactWrite)", () => {
    const messages: TranscriptEntry[] = [
      toolUse("c1", "AskUserQuestionWithForm", "2026-10-02T10:00:00Z", { title: "Q" }),
      toolUse("c2", "mcp__launchpad__ArtifactWrite", "2026-10-02T10:00:01Z", { title: "A" }),
      toolUse("c3", "Read", "2026-10-02T10:00:02Z", { file_path: "/x.ts" }),
    ];
    const calls = extractToolCallsByTurn(messages).get(TURN);
    expect(calls).toHaveLength(1);
    expect(calls![0].tool).toBe("Read");
  });

  it("keeps calls from different turns separate and tolerates missing results", () => {
    const messages: TranscriptEntry[] = [
      toolUse("c1", "Read", "2026-10-02T10:00:00Z", {}, "turn-a"),
      toolUse("c2", "Grep", "2026-10-02T10:01:00Z", {}, "turn-b"),
    ];
    const byTurn = extractToolCallsByTurn(messages);
    expect(byTurn.get("turn-a")![0].tool).toBe("Read");
    expect(byTurn.get("turn-b")![0].tool).toBe("Grep");
    expect(byTurn.get("turn-a")![0].output).toBeUndefined();
    expect(byTurn.get("turn-a")![0].elapsedMs).toBeUndefined();
  });
});

describe("buildMessageItems toolCalls attachment", () => {
  it("attaches the turn's calls to the response bubble sharing its turn_id", () => {
    const messages: TranscriptEntry[] = [
      { ts: "2026-10-02T10:00:00Z", role: "user", content: "do it", event_type: "message" },
      toolUse("c1", "Execute", "2026-10-02T10:00:01Z", { command: "ls" }),
      toolResult("c1", "2026-10-02T10:00:02Z", "ok\n"),
      response("Done.", "2026-10-02T10:00:03Z"),
    ];
    const { items } = buildMessageItems(
      messages,
      null,
      new Map(),
      extractToolCallsByTurn(messages)
    );
    const agentItems = items.filter((i) => i.type === "message" && typeof i.entry.role !== "string" && "agent" in i.entry.role);
    expect(agentItems).toHaveLength(1);
    expect(agentItems[0].toolCalls).toHaveLength(1);
    expect(agentItems[0].toolCalls![0].tool).toBe("Execute");
  });

  it("prefers the client-stamped metadata.tool_calls over turn pairing", () => {
    const stamped: TranscriptEntry = {
      ts: "2026-10-02T10:00:03Z",
      role: { agent: AGENT },
      content: "Done.",
      event_type: "message",
      metadata: { turn_id: TURN, tool_calls: [{ tool: "Read", input: { file_path: "/a" } }] },
    };
    const { items } = buildMessageItems([stamped], null, new Map(), new Map());
    expect(items[0].toolCalls).toHaveLength(1);
    expect(items[0].toolCalls![0].tool).toBe("Read");
  });
});

describe("ToolCallGroup", () => {
  function render(calls: Parameters<typeof ToolCallGroup>[0]["calls"]) {
    const container = document.createElement("div");
    document.body.appendChild(container);
    act(() => {
      createRoot(container).render(<ToolCallGroup calls={calls} />);
    });
    return container;
  }

  it("renders rows collapsed by default and expands one on click", () => {
    const container = render([
      { id: "c1", tool: "Execute", input: { command: "echo hi" }, output: "hi\n", elapsedMs: 1200 },
    ]);
    const row = container.querySelector('[data-testid="tool-call-row"]');
    expect(row).toBeTruthy();
    expect(row!.textContent).toContain("Ran echo hi");
    expect(row!.textContent).toContain("1s");
    // Collapsed: no input/output panes.
    expect(container.querySelector('[data-testid="tool-call-output"]')).toBeNull();

    act(() => {
      (row!.querySelector("button") as HTMLButtonElement).click();
    });
    expect(container.querySelector('[data-testid="tool-call-output"]')!.textContent).toContain("hi");
    expect(container.querySelector('[data-testid="tool-call-input"]')!.textContent).toContain("echo hi");
  });

  it("collapses more than 3 calls behind a group header", () => {
    const calls = Array.from({ length: 5 }, (_, i) => ({ id: `c${i}`, tool: "Read" }));
    const container = render(calls);
    expect(container.textContent).toContain("5 tool calls");
    expect(container.querySelectorAll('[data-testid="tool-call-row"]')).toHaveLength(0);

    act(() => {
      (container.querySelector("button") as HTMLButtonElement).click();
    });
    expect(container.querySelectorAll('[data-testid="tool-call-row"]')).toHaveLength(5);
  });

  it("auto-expands the running (in-focus) call and collapses it once done — the droid TUI behavior", () => {
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    // While running: expanded with its input visible, no click needed.
    act(() => {
      root.render(<ToolCallGroup calls={[{ id: "c1", tool: "Execute", input: { command: "make test" }, running: true }]} />);
    });
    expect(container.querySelector('[data-testid="tool-call-input"]')!.textContent).toContain("make test");

    // Completion: the same row collapses itself and the output is tucked away.
    act(() => {
      root.render(<ToolCallGroup calls={[{ id: "c1", tool: "Execute", input: { command: "make test" }, output: "ok", elapsedMs: 800 }]} />);
    });
    expect(container.querySelector('[data-testid="tool-call-input"]')).toBeNull();
    expect(container.querySelector('[data-testid="tool-call-output"]')).toBeNull();

    // Clicking the collapsed row still expands it manually.
    act(() => {
      (container.querySelector('[data-testid="tool-call-row"] button') as HTMLButtonElement).click();
    });
    expect(container.querySelector('[data-testid="tool-call-output"]')!.textContent).toContain("ok");
  });

  it("a manual toggle wins over the running flag", () => {
    const container = render([{ id: "c1", tool: "Execute", input: { command: "ls" }, running: true }]);
    // Auto-expanded while running; user collapses it.
    act(() => {
      (container.querySelector('[data-testid="tool-call-row"] button') as HTMLButtonElement).click();
    });
    expect(container.querySelector('[data-testid="tool-call-input"]')).toBeNull();
  });

  it("marks running and failed calls distinctly", () => {
    const container = render([
      { id: "c1", tool: "Read", running: true },
      { id: "c2", tool: "Execute", isError: true, output: "boom" },
    ]);
    // Running call: pulsing blue dot; failed call: red dot.
    expect(container.querySelector(".bg-blue-400.animate-pulse")).toBeTruthy();
    expect(container.querySelector(".bg-red-500")).toBeTruthy();
  });
});
