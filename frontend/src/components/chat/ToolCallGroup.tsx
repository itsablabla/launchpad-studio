/**
 * ToolCallGroup — droid-TUI-style tool-call transcript for a single turn.
 *
 * Each tool call renders as a collapsed one-line row (status dot + friendly
 * label via `describeToolCall` + elapsed time). Clicking a row expands it to
 * show the call's input and output — the same expand/collapse affordance the
 * droid TUI gives every tool call. When a turn used more than
 * `GROUP_COLLAPSE_THRESHOLD` tools, the rows themselves start hidden behind a
 * "N tool calls" header so a tool-heavy turn doesn't dominate the transcript.
 *
 * Data comes from two sources that share this component:
 *  - live: the chat store's per-turn in-flight records (StreamingMessage)
 *  - history: `tool_use`/`tool_result` transcript entries paired by
 *    `tool_use_id` (MessageList → MessageBubble)
 */
import { useState } from "react";
import { ChevronRight, Wrench } from "lucide-react";
import { describeToolCall, type ToolCallRecord } from "./toolCallLabel";

export type { ToolCallRecord } from "./toolCallLabel";

const GROUP_COLLAPSE_THRESHOLD = 3;
const OUTPUT_PREVIEW_MAX = 4000;
const INPUT_PREVIEW_MAX = 4000;

function formatElapsedMs(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  return `${Math.floor(s / 60)}m ${s % 60}s`;
}

function stringifyDetail(value: unknown): string {
  if (value == null) return "";
  const s =
    typeof value === "string"
      ? value
      : (() => {
          try {
            return JSON.stringify(value, null, 2);
          } catch {
            return String(value);
          }
        })();
  // A file-write input can be a whole file — cap what lands in the DOM.
  return s.length > INPUT_PREVIEW_MAX ? s.slice(0, INPUT_PREVIEW_MAX) + "\n… (truncated)" : s;
}

function ToolCallRow({ call }: { call: ToolCallRecord }) {
  // droid-TUI focus behavior: the running call is expanded while it's in
  // focus and collapses itself the moment it completes — no click needed.
  // `manualOpen` is the user's override: once they click, their choice
  // sticks (null = follow the running flag).
  const [manualOpen, setManualOpen] = useState<boolean | null>(null);
  const open = manualOpen ?? !!call.running;
  const { label, detail } = call.running
    ? describeToolCall(call.tool, call.input, false)
    : describeToolCall(call.tool, call.input, true);
  const hasDetail = (call.input && Object.keys(call.input).length > 0) || !!call.output;

  return (
    <div data-testid="tool-call-row" className="text-[12px] leading-[18px]">
      <button
        type="button"
        disabled={!hasDetail}
        onClick={() => setManualOpen(!open)}
        className={`flex items-center gap-[6px] w-full text-left rounded-[4px] px-[4px] py-[2px] -mx-[4px] ${
          hasDetail ? "hover:bg-[var(--bg-hover)] cursor-pointer" : "cursor-default"
        }`}
      >
        <ChevronRight
          className={`w-[11px] h-[11px] shrink-0 text-[var(--text-tertiary)] transition-transform ${open ? "rotate-90" : ""} ${hasDetail ? "" : "opacity-0"}`}
        />
        <span
          className={`w-[6px] h-[6px] rounded-full shrink-0 ${
            call.running ? "bg-blue-400 animate-pulse" : call.isError ? "bg-red-500" : "bg-green-500"
          }`}
        />
        <span className="text-[var(--text-secondary)] truncate">{label}</span>
        {detail && <span className="text-[var(--text-tertiary)] truncate italic">{detail}</span>}
        {call.elapsedMs != null && (
          <span className="ml-auto pl-[8px] text-[10px] text-[var(--text-tertiary)] shrink-0 tabular-nums">
            {formatElapsedMs(call.elapsedMs)}
          </span>
        )}
      </button>
      {open && hasDetail && (
        <div className="ml-[21px] mt-[2px] mb-[4px] flex flex-col gap-[4px]">
          {call.input && Object.keys(call.input).length > 0 && (
            <pre
              data-testid="tool-call-input"
              className="text-[11px] leading-[15px] text-[var(--text-secondary)] bg-[var(--bg-tertiary,var(--modal-bg-tertiary))] rounded-[6px] px-[8px] py-[6px] overflow-x-auto whitespace-pre-wrap break-words max-h-[180px] overflow-y-auto"
            >
              {stringifyDetail(call.input)}
            </pre>
          )}
          {call.output && (
            <pre
              data-testid="tool-call-output"
              className={`text-[11px] leading-[15px] rounded-[6px] px-[8px] py-[6px] overflow-x-auto whitespace-pre-wrap break-words max-h-[240px] overflow-y-auto ${
                call.isError ? "text-red-400 bg-red-500/10" : "text-[var(--text-secondary)] bg-[var(--bg-tertiary,var(--modal-bg-tertiary))]"
              }`}
            >
              {call.output.length > OUTPUT_PREVIEW_MAX
                ? call.output.slice(0, OUTPUT_PREVIEW_MAX) + "\n… (truncated)"
                : call.output}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}

export function ToolCallGroup({ calls, live = false }: { calls: ToolCallRecord[]; live?: boolean }) {
  const [expanded, setExpanded] = useState(false);
  if (calls.length === 0) return null;

  // Small turns show their rows directly, like the droid TUI. So do live
  // turns of any size while a call is running — the in-focus row must never
  // be hidden behind the header. Once the user has opened the group header,
  // keep the rows mounted for the rest of the live turn: collapsing the
  // instant the last running call completes would yank every row (and its
  // expand state) out from under the viewer mid-read.
  const anyRunning = calls.some((c) => c.running);
  if (calls.length <= GROUP_COLLAPSE_THRESHOLD || (live && (anyRunning || expanded))) {
    return (
      <div data-testid="tool-call-group" className="mt-[6px] flex flex-col gap-[1px]">
        {calls.map((call, i) => (
          <ToolCallRow key={call.id ?? i} call={call} />
        ))}
      </div>
    );
  }

  const errorCount = calls.filter((c) => c.isError).length;
  const runningCount = calls.filter((c) => c.running).length;
  return (
    <div data-testid="tool-call-group" className="mt-[6px]">
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="flex items-center gap-[6px] text-[12px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] rounded-[4px] px-[4px] py-[2px] -mx-[4px] cursor-pointer"
      >
        <ChevronRight className={`w-[11px] h-[11px] text-[var(--text-tertiary)] transition-transform ${expanded ? "rotate-90" : ""}`} />
        <Wrench className="w-[11px] h-[11px] text-[var(--text-tertiary)]" />
        <span>
          {calls.length} tool calls
          {runningCount > 0 ? ` (${runningCount} running)` : ""}
          {errorCount > 0 ? ` — ${errorCount} failed` : ""}
        </span>
      </button>
      {expanded && (
        <div className="mt-[2px] flex flex-col gap-[1px]">
          {calls.map((call, i) => (
            <ToolCallRow key={call.id ?? i} call={call} />
          ))}
        </div>
      )}
    </div>
  );
}
