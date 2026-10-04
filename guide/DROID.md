# Droid CLI Integration — How It Works

This document describes how Launchpad Studio runs the `droid` CLI as an agent
provider: the run lifecycle, prompt composition, session resume, the Telegram
bridge path, failure handling, and the operational quirks that matter when
developing against it.

Everything below describes the droid-e2e reference agent, but the mechanics
apply to any agent whose profile uses `provider.type: Cli` with
`normalizer: droid`.

---

## 1. Agent profile

An agent is a YAML file under `<data_root>/agents/<id>.yaml`. The droid-e2e
profile, annotated:

```yaml
id: droid-e2e
provider:
  type: Cli
  command: /Users/jadengarza/.local/bin/droid   # the CLI binary
  args: [exec, --output-format, stream-json, --skip-permissions-unsafe]
  normalizer: droid                  # stream-json → internal event stream
  output_format: StreamJson
  input_mode: Arg                    # prompt passed as an argv argument
  model_arg: -m                      # model flag
  system_prompt_arg: --append-system-prompt   # composed prompt appended to droid's own
  session_arg: -s                    # session resume flag
  no_output_timeout_ms: 90000        # watchdog: kill if silent for 90s
model: custom:garza-auto             # Garza router alias
runner_mode: cli
serialize: true                      # one run at a time per agent
max_instances: 1
timeout_seconds: 600
working_dir: /tmp/lp-droid-e2e       # cwd the CLI runs in
persona: You are Droid E2E, a verification agent.
special_instructions: Answer the user's request directly, using tools when the task needs them.
minimal_prompt: true                 # omit Launchpad's platform behavior blocks
channels:
  - binding_id: telegram
    kind: telegram
    enabled: true
    kind_config: { type: Telegram, bot_username: garza_droid_bot, thread_mode: dedicated }
```

Two flags deserve special attention:

- **`--append-system-prompt`** (not `--system-prompt`): droid has its own
  built-in CLI system prompt that cannot be removed. Launchpad's composed
  prompt is *appended* to it. This is why the agent always has two
  instruction sources; `minimal_prompt: true` shrinks the Launchpad side so
  the two don't fight (see §3).
- **`session_arg: -s`**: enables cross-turn session resume (see §4).

## 2. Run lifecycle

A message (from the UI composer, the Telegram bridge, or an assignment)
flows through these stages:

```
enqueue ──► queue_manager ──► request prepare ──► argv build ──► spawn
                                                              │
   transcript/SSE ◄── finalize ◄── stream loop ◄── supervisor─┘
```

1. **Enqueue.** `POST /agents/{id}/messages` (or the Telegram inbound
   poller) puts the message on the agent's in-memory queue.
   **The queue is not persisted** — a process restart drops queued and
   in-flight messages (observed: a `tauri dev` watcher restart mid-run lost
   two queued messages with no requeue).

2. **Queue manager.** With `serialize: true`, runs execute one at a time;
   a second message waits for the first run's `RunEnded`. Heartbeats log
   `queue_depth` and `running_count` every 5s.

3. **Request prepare.** Composes the system prompt (§3), selects the
   history slice (§3.1), resolves the model alias, and logs
   `system_prompt_chars` — the first place to look when a prompt grows
   unexpectedly.

4. **Argv build.** Produces the final command line:

   ```
   droid exec --output-format stream-json --skip-permissions-unsafe \
     -m custom:garza-auto \
     --append-system-prompt "<composed prompt>" \
     [-s <session-id>] \
     "<[Conversation history]…[Current message]…>"
   ```

   The session pair is spliced *before* the prompt so the prompt stays the
   last free argument (`apply_session_arg`, pinned by
   `apply_session_arg_keeps_prompt_last_for_arg_mode`).

5. **Per-agent Factory home.** Before spawn, `materialize_droid_factory_home`
   creates `<agent_home>/factory-home` and copies auth and settings from the
   user's real `~/.factory` (auth/settings only, refreshed when stale). The
   child gets `FACTORY_HOME_OVERRIDE=<agent_home>/factory-home`, so droid
   writes its sessions, logs, and MCP config under the agent home instead of
   polluting the user-level home. If the real home can't be found, the hook
   returns `None` and droid runs with its default home — the byte-cap check
   (§4) silently skips in that configuration, by design.

6. **MCP config merge.** A per-run `.factory/mcp.json` is written into the
   run cwd pointing droid at Launchpad's MCP endpoint (the 67-tool
   `launchpad___*` surface), merged with any existing config via an atomic
   temp-file-then-rename write. The repo the cwd belongs to gets
   `/.factory/mcp.json` added to its `.gitignore` automatically — the
   gitignore writer walks up to the repo root and anchors the rule, so
   subdirectory cwds are covered too.

7. **Spawn + stream.** The supervisor spawns the process; stdout is
   stream-json, normalized by `ao-normalizer/src/droid.rs` into internal
   events (`TextDelta`, `ToolCall`, `ThinkingStarted/…`, `Usage`, …) that
   drive both the transcript and the SSE feed to the UI.

8. **Finalize.** On process exit, `classify_end_reason` maps the
   termination to a run outcome. **Only exit code 0 is `Completed`.** The
   supervisor reports `Natural` for *any* self-exit, so without this check a
   crashed turn looked successful — that was the bug behind "silent"
   failures: a nonzero exit now produces `Error`, clears the stored session
   id (un-wedging stale resumes), and raises a visible error bubble. A
   `bg_error_emitted` flag suppresses double-reporting when the provider
   already streamed an error event.

## 3. Prompt composition

The composed system prompt (`system_prompt_composer`) has gated sections:

| Section | Content | `minimal_prompt: true` |
|---|---|---|
| Identity / run-context | cwd, os, model, date, project key | kept |
| Persona | profile `persona` | kept |
| Special instructions | profile `special_instructions` | kept |
| Memories | recalled memories, as data | kept |
| §5 Baseline guidance | ~500 lines of platform behavior rules | **omitted** |
| §5b CLI tool preference | Launchpad-tools-over-CLI rules | **omitted** |
| §9 Memory-save instruction | when to write memories | **omitted** |

With `minimal_prompt: true` the composed prompt is ~1,600 chars (verified in
the spawn logs: `system_prompt_chars=1596`).

**Why this exists:** droid's built-in CLI prompt pushes a terse, fragmentary
style. The original `special_instructions` pushed the opposite ("full
detailed answers"), and the agent ended up reporting "conflicting style
directives" — then kept repeating that claim on later turns because the
claim itself lived in the resumed session history and the injected
transcript (a stale echo, not a live conflict). The fix was to slim the
Launchpad side to one neutral line, not to fight droid's prompt harder.

### 3.1 History injection

UI/thread turns prepend transcript history to the user message:

```
[Conversation history]
[HH:MM] user: …
[HH:MM] droid-e2e: <tool_use id="…" name="…">{…}</tool_use>
[HH:MM] tool: <tool_result tool_use_id="…">…</tool_result>
…
[Current message]
<the actual message>
```

Budgets (`context.rs`): tool inputs capped at 600 chars, tool outputs at
1200 chars, total history at 12,000 chars, newest messages win. Note this
caps *injected* history only — the resumed session (§4) is a separate,
much larger replay channel.

## 4. Session resume

Droid persists each conversation as a JSONL session file under
`$FACTORY_HOME_OVERRIDE/.factory/sessions/<cwd-slug>/<session-id>.jsonl`.
Launchpad stores the latest session id per `(command, thread)` in
`<agent_home>/cli-sessions.json`:

```json
{
  "sessions": {
    "/Users/jadengarza/.local/bin/droid:default": { "id": "860cea45-…", "turns": 5 },
    "/Users/jadengarza/.local/bin/droid:1ee19992-…": { "id": "79f79a11-…", "turns": 5 }
  }
}
```

- **Keying is per thread.** The UI main thread (`:default`) and each
  Telegram mirror thread resume their own sessions; they never share one.
- **Format:** `{"id", "turns"}`; a legacy bare-string value is accepted as
  `turns: 0`.
- **Turn cap:** re-storing the same id increments `turns`; a new id resets
  it. At `CLI_SESSION_MAX_RESUME_TURNS = 40` the loader refuses the id and
  the next turn starts fresh.
- **Byte cap:** `CLI_SESSION_MAX_RESUME_BYTES = 256KB`. Before resuming,
  the loader stats the session file (scanning the one level of cwd-slug
  dirs under the factory home — ids are UUIDs, so the scan is
  collision-free and doesn't recompute droid's path mangling). Over the
  cap → fresh session, logged:
  `CLI session exceeds the byte cap — starting a fresh session instead of resuming`.
  A *missing* file keeps the old behavior (return the id) so the fallback
  default-home configuration isn't broken; a genuinely stale id is caught
  by exit-code classification, which clears the pointer.

**Why both caps:** resume replays the *entire* session through the provider
on every model call, and a tool-heavy turn (email dumps, big file reads)
can add hundreds of KB in one exchange. Measured on droid-e2e: a 1.1MB
session produced **3.2M input tokens** on a single two-tool turn; after the
byte cap forced a fresh session, the same kind of turn used **62K**. The
turn cap alone never bit — byte growth outruns turn count by orders of
magnitude.

Trade-off: crossing either cap silently starts a fresh session, so the
agent loses conversational continuity beyond what `[Conversation history]`
injects (12K chars). Resume is an optimization, never a requirement.

## 5. Telegram bridge

Detail lives in `TELEGRAM.md`; the droid-relevant parts:

- **Token resolution order:** env var `LAUNCHPAD_TELEGRAM_TOKEN__<AGENT_ID>`
  → shared secret vault (OS keychain, or `secret_vault.json` under the data
  root when the keychain is unreachable or
  `LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK` is set).
  **Backend selection is per process.** A process that selects keychain
  does not see tokens in the file and vice versa — this caused a
  "disconnected" bridge after a relaunch whose keychain item was empty
  while the token lived in the file vault.
- **Inbound:** one long-poll `getUpdates` loop per binding. The offset
  cursor (`channel_cursors/<agent>__<binding>.json`) is persisted after
  each handled update, bounding redelivery to at most the single update in
  flight during a crash. Messages that arrive while no poller is up queue
  server-side at Telegram and are delivered when polling resumes.
- **Threading:** each distinct `chat_id` mints its own mirror thread on
  demand (`thread_mode: dedicated`); the thread appears in the Chat UI with
  a "mirrors your Telegram conversation" composer.
- **Outbound:** an observer watches run events; typing heartbeats fire
  during the run, and the final text is relayed at `RunEnded` (markdown →
  Telegram HTML, plain-text retry on a 400). Replies to dead mapping are
  dropped, never misrouted.
- **Leases:** `channel_leases/` prevents two processes from polling the same
  bot (Telegram 409 conflicts otherwise).

## 6. Failure handling

- **90s no-output watchdog** kills a wedged run (`no_output_timeout_ms`).
- **Silent-failure bubble:** a failed run that produced no reply text gets
  a visible Error bubble in the UI instead of going quietly dark.
- **Stale resume wedge:** a session id the provider rejects exits nonzero →
  classified `Error` → pointer cleared → next turn starts fresh. Before
  exit-code classification this wedged the thread forever.
- **Stream robustness (normalizer):** inline `<thinking>` blocks are split
  out of text (with the unclosed-tail withheld and flushed at finalize);
  duplicate `tool_call` ids are deduped so the watchdog counter can't be
  poisoned; tool-only turns still finalize and surface.
- **Atomic writes:** `cli-sessions.json` and `.factory/mcp.json` are
  written temp-then-rename, so a crash mid-write can't corrupt them.

## 7. Performance profile

Measured on droid-e2e (Garza router, `custom:garza-auto`):

| Turn shape | Wall time | Notes |
|---|---|---|
| Fresh session, simple question | ~7–14s | generation-dominated |
| Resumed 950KB–1.1MB session | ~60s+ | replay inflates every model call |
| Resumed, tool-heavy (mail check) | ~2min | per-call retransmit compounds |
| Post-byte-cap fresh mail check (4 tool calls) | ~117s, 62K input tokens | 1s spawn-to-first-event |

Phase timing from logs: queue→spawn→MCP init→first stream event ≈ 1s.
Everything else is gateway generation. Consequences:

- **Prompt size is the lever we control** — byte cap (done), history
  injection caps (already in place), MCP tool-surface trimming (67 tools
  listed per turn; not yet done).
- **Model choice is the lever we don't** — routing simple turns to a
  faster model behind the Garza router is the next big win.
- `droid -r low` (reasoning effort) measured *no* improvement on this
  gateway.

## 8. Operational quirks (read before debugging)

1. **`tauri dev` watches `crates/`.** Saving any Rust source — including a
   test file or an editor temp file — rebuilds and restarts the app,
   killing in-flight runs and dropping the in-memory queue. Batch edits;
   restart deliberately.
2. **Keychain grants are per-signature.** Dev builds are ad-hoc signed;
   every rebuild can re-trigger keychain prompts for vault access. With no
   Apple Development identity on the machine there is no stable-signing
   workaround; answer the prompt once per rebuild.
3. **`LAUNCHPAD_STUDIO_DATA_DIR` pins the profile.** When set (the tools
   worktree flow), the Workspaces UI disables switching. Unset → default
   root `~/.launchpad_studio`. Agents, threads, and vault contents do NOT
   follow across roots; migrating an agent means recreating it via
   `POST /agents`, re-setting channel tokens via
   `PUT /agents/{id}/telegram/token`, and copying `agent_homes/<id>/`.
4. **Timing harness:** `/tmp/droid-speed.sh`-style measurement (copy the
   factory home, `FACTORY_HOME_OVERRIDE`, wall + TTFB across variants) is
   the reliable way to compare configurations; the app log's
   `cache usage` lines (input/output/cache_read tokens per run) are the
   first thing to check on any "it's slow" report.
