# Droid CLI Integration Feedback

**To:** Factory engineering team
**From:** Launchpad Studio (open-source agent orchestrator; Tauri desktop app)
**Droid version:** 0.232.0 (`~/.local/bin/droid`)
**Platform:** macOS 25.4.0, arm64
**Logs referenced:** `~/.factory/logs/droid-log-single.log`

## Context

Launchpad Studio drives `droid exec --output-format stream-json` as a
subprocess per chat turn, and delivers its MCP server to droid via the
project-level `.factory/mcp.json` in the spawn cwd. The findings below
were verified empirically on a real user machine (~26 configured MCP
servers, several slow/hanging) and cross-checked against a minimal
Factory home containing a single localhost MCP server.

---

## 1. `--settings <path>` silently ignores `mcpServers`

**Symptom.** A per-process settings file containing `{"mcpServers": {...}}`
is accepted without error, but the servers are never discovered or
connected. Droid's own McpHub log shows no connection attempt for them.

**Expected vs actual.**
- Expected: settings-file `mcpServers` merge into the effective MCP
  config (like every other settings key), **or** droid rejects the key
  with a clear error at startup.
- Actual: the file parses cleanly, the run proceeds, and the servers
  silently do not exist.

**Reproduction.**

```bash
cat > /tmp/mcp-settings.json <<'EOF'
{"mcpServers": {"probe": {"command": "node", "args": ["./probe-mcp.js"]}}}
EOF
droid exec --settings /tmp/mcp-settings.json --output-format stream-json \
  "what MCP servers do you have?"
```

**Evidence.** No `Connecting to MCP server probe` / `Registered MCP tool`
lines appear in `droid-log-single.log`; McpHub logs only servers from
user/project config sources.

**Impact.** Embedders have no per-invocation MCP delivery channel. The
only working option is writing `.factory/mcp.json` into the user's
project directory, which pollutes the repo (git noise — Factory's own
docs warn project `mcp.json` gets committed) and races when two
concurrent spawns share a cwd.

**Suggested fix.** Honor `mcpServers` in `--settings`, or add a
dedicated `--mcp-config <path>` flag (as the `claude` CLI does).

---

## 2. Exec-mode tool catalog is snapshotted before MCP registration completes

**Symptom.** In `droid exec`, the model-visible tool catalog is frozen
at run start, but MCP servers finish connecting and registering
**after** the first model turn begins.

**Expected vs actual.**
- Expected: by the time the first model request is sent, MCP tools are
  registered (or the catalog can change mid-conversation).
- Actual: the first model turn runs without MCP tools on machines with
  a realistic roster.

**Evidence (timeline, real user machine, ~26 configured MCP servers, many slow/hanging).**
- `+0s` — spawn
- `+1s` — stream-json `system` init event emitted
- `+18s` — model's first answer reports the MCP tools unavailable;
  ToolSearch queries for them fail with `No deferred tools matched`
- `+48s` — `Registered MCP tool mcp_launchpad_*` log lines appear
- Metrics line logs `midConversationToolChangesActive: false`

**Control.** With a minimal Factory home (single localhost MCP server),
registration beat the first turn and the tools were callable — so the
bug is a race, not a total failure.

**Impact.** On real user machines (large MCP rosters are the norm for
power users), exec-mode MCP tools are effectively unusable, **and**
every run still pays the full roster connection latency (30–60s+)
before the first useful turn.

**Suggested fixes (any of).**
1. Gate the first model request on MCP registration completing (or on
   a short settle timeout).
2. Enable mid-conversation tool changes for exec mode.
3. Register fast/localhost servers first.
4. Connect servers lazily on ToolSearch `select:`.

---

## 3. MCP tool discoverability and ToolSearch shape in exec mode

**Symptom.** MCP tools register with names like `mcp_<server>_<tool>`
(0.225-era) / `server___tool` (0.232), but the model only learns about
deferred tools via ToolSearch — and the deferred catalog it sees did
not include freshly-registered MCP tools (see #2). Even when tools
were registered, the model twice emitted ToolSearch calls as literal
text — `<tool_use name="ToolSearch">...` appearing in the assistant
text stream — instead of executing them.

**Expected vs actual.**
- Expected: exec-mode sessions expose ToolSearch as a first-class
  callable tool exactly as the TUI does, and the deferred catalog
  reflects MCP tools once registered.
- Actual: the deferred catalog lags MCP registration, and ToolSearch
  invocation is inconsistent enough that the model falls back to
  emitting pseudo-tool XML as text.

**Impact.** Even when the #2 race is won, MCP tools are unreliable in
exec mode: discoverability depends on an invisible catalog and a tool
whose availability/shape appears inconsistent in stream-json sessions.

**Suggested fix.** Include the deferred-tools list (or at minimum
server names + tool counts) in the exec `system`/init payload, and
make ToolSearch a first-class callable tool in exec exactly as in TUI.

---

## 4. Cloud sync 401 retries block/fail headless runs without auth

**Symptom.** In one environment with no Factory auth token, `droid exec`
retried `Cloud sync session create` against `FACTORY_API_BASE_URL`
(receiving 401) several times before proceeding; the run ultimately
failed with exit code 1 and a stream error event.

**Expected vs actual.** In a headless/embedder context, cloud sync
should degrade gracefully — skip it — rather than retry-block the run
or contribute to its failure.

**Suggested fix.** On 401/missing credentials in exec mode, skip cloud
sync immediately (single log line) and continue the local session.

---

## 5. Request: document the `stream-jsonrpc` protocol

**Context.** `droid exec --input-format stream-jsonrpc
--output-format stream-jsonrpc` starts a persistent session process
(protocol `factoryApiVersion 1.0.0` / `factoryProtocolVersion
1.244.0`). For embedders this is the ideal shape: one long-lived
process per conversation instead of one cold-starting subprocess per
turn (each cold start pays full MCP roster connection before the first
event — see #2).

**Observed.** The method surface is discoverable from the binary
(`droid.add_user_message`, `droid.load_session`, `droid.close_session`,
`droid.interrupt_session`, `droid.update_session_settings`,
`droid.session_notification`, …) and notifications stream correctly,
but every request we constructed was rejected with
`-32700 Invalid JSON-RPC message` regardless of envelope shape (bare
`{"jsonrpc":"2.0","id":1,"method":...}`, or with `factoryApiVersion`
added). Without a schema for the request envelope and method params,
the mode is unusable.

**Request.** Publish the stream-jsonrpc request schema (envelope
fields, method params, session lifecycle) or a minimal worked example
of opening a session and submitting a user message.

---

## What works well

- The stream-json event stream is well-formed and complete: init with
  `session_id`, message snapshots, reasoning events,
  `tool_call`/`tool_result` pairing, and completion with usage
  including `factory_credits`.
- Project-level `.factory/mcp.json` discovery and hot reload work as
  documented.
- `--append-system-prompt` composes cleanly with droid's own system
  prompt.
- `--skip-permissions-unsafe` plus exit code 2 on unknown flags makes
  headless embedding predictable and safely scriptable.

---

## Environment summary

| Item | Value |
|---|---|
| droid | 0.232.0 (`~/.local/bin/droid`) |
| OS | macOS 25.4.0, arm64 |
| Invocation | `droid exec --output-format stream-json` (subprocess per turn) |
| MCP delivery | project-level `.factory/mcp.json` in spawn cwd |
| MCP roster (real env) | ~26 configured servers, several slow/hanging |
| MCP roster (control env) | 1 localhost server |
| Logs | `~/.factory/logs/droid-log-single.log` |

Full logs, the settings fixtures, and the stream-json captures used
for this report are available on request.
