/** Shared tool-call chip label logic — imported by all chat surfaces. */

/** One tool call in a turn's transcript, shared by the live in-flight view
 *  (chatStore) and the persisted-history view (tool_use/tool_result
 *  transcript pairing). Rendered by `ToolCallGroup`. */
export interface ToolCallRecord {
  /** `tool_use_id` when known (history always has it; live classic chips don't). */
  id?: string;
  tool: string;
  input?: Record<string, unknown>;
  output?: string;
  isError?: boolean;
  /** Wall-clock duration when known. */
  elapsedMs?: number;
  /** True while the call is still running (live view only). */
  running?: boolean;
}

/** Strip MCP namespacing (`mcp__<server>__`) from a tool name so chips show
 *  the underlying tool rather than the transport. Handles nested forms like
 *  `mcp__launchpad__mcp__everything__echo` → `echo`. */
export function stripMcpPrefix(tool: string): string {
  let s = tool;
  while (s.startsWith("mcp__")) {
    const sep = s.indexOf("__", 5);
    if (sep < 0) break;
    s = s.slice(sep + 2);
  }
  return s;
}

/** Tool names that get their own richer rendering and must NOT also appear
 *  as generic tool rows: forms become FormAnswerBubbles (preprocessFormToolPairs)
 *  and ArtifactWrite renders as the inline artifact card. Applied everywhere a
 *  ToolCallRecord is produced — live stamp, salvage paths, history pairing —
 *  so the live view and the refetched view contain the same rows. */
export const TOOL_ROW_SKIP = new Set(["AskUserQuestionWithForm", "ArtifactWrite"]);

/** True when `tool` (MCP-qualified or bare) belongs to TOOL_ROW_SKIP. */
export function isToolRowSkipped(tool: string): boolean {
  return TOOL_ROW_SKIP.has(stripMcpPrefix(tool));
}

/** True for task/agent output files (e.g. `.../tasks/<id>/output...`). */
export function isAgentOutputPath(path: string): boolean {
  return /\/tasks\/[^/]+\/output/.test(path);
}

function truncate(s: string): string {
  return s.length > 28 ? s.slice(0, 25) + "…" : s;
}

/** Map tool names to human-friendly chip labels, enriched by tool input. */
export function describeToolCall(
  rawTool: string,
  input?: Record<string, unknown>,
  completed?: boolean,
): { label: string; detail?: string } {
  const tool = stripMcpPrefix(rawTool);

  if (tool === "Delegate") {
    const rawTarget = input?.target;
    const target =
      typeof rawTarget === "string" && rawTarget.trim().length > 0
        ? rawTarget.trim()
        : null;
    if (target == null) return { label: completed ? "Delegated" : "Delegating…" };
    return { label: completed ? `Delegated to ${target}` : `Delegating to ${target}…` };
  }

  if (tool === "Agent") {
    const desc = (input?.description as string) ?? (input?.prompt as string);
    if (desc) return { label: "Using Agent", detail: desc };
    return { label: "Using Agent" };
  }

  if (tool === "RunSkill") {
    const skill = input?.skill as string | undefined;
    if (skill) return { label: `Loading skill: ${skill}` };
    return { label: "Loading skill" };
  }

  if (tool === "TodoCreate") {
    if (completed) return { label: "Used TodoList" };
    const name = input?.name as string | undefined;
    return { label: name ? `Using TodoList: ${name}` : "Using TodoList" };
  }

  const memoryLabels: Record<string, { pending: string; settled: string }> = {
    MemoryWrite: { pending: "Saving memory…", settled: "Saved memory" },
    MemoryEdit: { pending: "Editing memory…", settled: "Edited memory" },
    MemoryDelete: { pending: "Deleting memory…", settled: "Deleted memory" },
    MemoryList: { pending: "Listing memories…", settled: "Listed memories" },
  };
  if (tool in memoryLabels) {
    const { pending, settled } = memoryLabels[tool];
    return { label: completed ? settled : pending };
  }

  const filePath = (input?.file_path as string) ?? (input?.path as string);

  if (tool === "Read") {
    const verb = completed ? "Read" : "Reading";
    if (!filePath) return { label: verb };
    if (isAgentOutputPath(filePath)) return { label: `${verb} agent output` };
    return { label: `${verb} ${truncate(filePath.split("/").pop() ?? filePath)}` };
  }

  // droid's patch tool is `ApplyPatch`; its Edit matches claude's.
  if (tool === "Edit" || tool === "ApplyPatch") {
    const verb = completed ? "Edited" : "Editing";
    if (!filePath) return { label: verb };
    return { label: `${verb} ${truncate(filePath.split("/").pop() ?? filePath)}` };
  }

  // droid's file-write tool is `Create` (claude's is `Write`).
  if (tool === "Write" || tool === "Create") {
    const verb = completed ? "Created" : "Creating";
    if (!filePath) return { label: verb };
    return { label: `${verb} ${truncate(filePath.split("/").pop() ?? filePath)}` };
  }

  // droid's shell tool is `Execute` (claude's is `Bash`) — same labeling.
  if (tool === "Bash" || tool === "Execute") {
    const verb = completed ? "Ran" : "Running";
    const desc = (input?.description as string | undefined) ?? (input?.summary as string | undefined);
    if (desc) return { label: `${verb}: ${truncate(desc)}` };
    const cmd = input?.command as string | undefined;
    if (cmd) return { label: `${verb} ${truncate(cmd.split("/").pop() ?? cmd)}` };
    return { label: verb };
  }

  if (tool === "Grep") {
    const pattern = input?.pattern as string | undefined;
    if (pattern) return { label: `Searching for ${truncate(pattern)}` };
    return { label: "Searching" };
  }

  if (tool === "Glob") {
    const pattern = input?.pattern as string | undefined;
    if (pattern) return { label: `Finding files: ${truncate(pattern)}` };
    return { label: "Finding files" };
  }

  if (tool === "WebSearch") {
    const query = input?.query as string | undefined;
    if (query) return { label: `Searching the web: ${truncate(query)}` };
    return { label: "Searching the web" };
  }

  if (tool === "WebFetch") {
    const url = input?.url as string | undefined;
    if (url) {
      try {
        const domain = new URL(url).hostname;
        return { label: `Fetching ${truncate(domain)}` };
      } catch {
        return { label: "Fetching" };
      }
    }
    return { label: "Fetching" };
  }

  // droid's directory listing tool is `LS` (claude's is `ListDirectory`).
  if (tool === "ListDirectory" || tool === "LS") {
    if (filePath) return { label: `Browsing ${truncate(filePath.split("/").pop() ?? filePath)}` };
    return { label: "Browsing" };
  }

  return { label: `Using ${tool}` };
}
