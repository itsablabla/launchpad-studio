# Matrix Channel Integration — Design for Launchpad Studio

Goal: add **Matrix** as a first-class chat channel, on exactly the same footing as the
existing Telegram, Discord, Slack, and Email integrations — same supervisor, same
security model, same setup UX — not a bolt-on.

This document has three parts: (1) how the existing channel integrations actually work,
read from the source; (2) how those concepts map onto Matrix, cross-referenced against
the Matrix ecosystem's established bot patterns; (3) the concrete implementation design,
file by file.

---

## Part 1 — How channels work in this codebase (verified against source)

### 1.1 The protocol types (crate `ao-protocol`)

Every channel is data before it is code. Three enums/structs in
`crates/ao-protocol/src/agent.rs` define the whole model:

- **`ChannelKind`** (snake_case serde enum): `Telegram | Discord | Email | Slack |
WhatsApp | Webhook`. A closed enum — adding a channel kind is a protocol change, not
a plugin operation.
- **`ChannelBinding`** (one per channel on an agent profile): `binding_id`, `kind`,
`enabled`, `bridge_thread_id` (server-provisioned), `allowed_senders` (deprecated
inline list, now backfilled into `LinkedSenderStore`), `pending_pairing_code`, and
`kind_config`.
- **`ChannelKindConfig`** (tagged enum): only the kind-specific fields — e.g.
`Telegram { bot_username, thread_mode }`, `Discord { allowed_users, allowed_roles,
allowed_channels, ... }`, `Email { address, imap_host, ... }`.

Supporting types: `ChannelCursor` (per-kind dedup/resume state: Telegram's `offset`,
Discord's `session_id`/`seq`/`seen_message_ids`, Slack's `seen_event_ids`),
`ChannelConnectionState` (`Connected | Reconnecting | Disconnected | NotHoldingLease`),
and `MessageSource::Channel { kind, binding_id, conversation_id, sender_id }` stamped
onto every inbound `QueuedMessage`.

### 1.2 The transport seam (crate `ao-engine`, `channels/mod.rs`)

The entire channel architecture hangs on one crate-internal trait:

```rust
trait ChannelTransport: Send + Sync {
    fn kind(&self) -> ChannelKind;
    fn fingerprint(&self, agent: &AgentProfile, binding: &ChannelBinding) -> Option<String>;
    fn spawn(&self, ctx: ChannelRunContext, cancel: CancellationToken) -> JoinHandle<()>;
    fn invalidate_thread(&self, thread_id: &str);
    fn spawn_outbound_observer(self: Arc<Self>, ...) -> Option<JoinHandle<()>>;
}
```

- `fingerprint()` folds binding config + resolved secret into an opaque change-detection
string; `None` means "not runnable" (e.g. no token on file).
- `spawn()` runs the binding's inbound loop until cancelled.
- `spawn_outbound_observer()` starts the one-per-process reply relay for that kind
(Email returns `None` — it replies via a `SendEmail` tool, not an automatic relay).

A `ChannelTransportRegistry` maps `ChannelKind → Arc<dyn ChannelTransport>`; the
supervisor dispatches over `values()` so **a newly registered kind is picked up
automatically** — that is the registration point a Matrix transport plugs into.

### 1.3 The supervisor (`telegram/bridge.rs` — `ChannelBridge`)

One `ChannelBridge` task per process. Every 5 s (`RECONCILE_INTERVAL`) it:

1. Lists agent profiles; computes the *desired* set of (agent, binding) pairs that are
enabled and have a resolvable fingerprint.
2. Claims a **single-writer lease** per binding (`ChannelLeaseStore::try_claim`, TTL =
3× reconcile interval) so two processes sharing a data root can never both poll and
both reply to the same chat; a lost lease stops the task and reports
`NotHoldingLease`.
3. Starts/stops per-binding inbound tasks as the desired set and fingerprints change
(token rotation = fingerprint change = restart).
4. Maintains `ConnectionStateRegistry` (what `GET /agents/{id}/channels` reads back)
and the `LeaseGate` (the process-local "may I actually send?" check every outbound
relay consults before sending).

### 1.4 The inbound pipeline (Telegram as the canonical example)

`telegram/transport.rs::run_bot_poll_loop` → `handle_update`:

1. **Long-poll** `getUpdates` with a persisted `offset` cursor
(`ChannelCursorStore`), restored on startup so a restart resumes instead of
re-serving history; persisted after each update is fully handled (crash window =
one update).
2. **Pairing gate first**: `/start <code>` (also `/start@botname <code>` in groups) →
`try_link_chat` → writes `LinkedSenderStore` (never the whole profile document —
that race was deliberately closed). Replies with a success/failure message.
3. **Allow-list gate**: reject-all by default — an empty linked-sender list delivers
nothing. This is the documented anti-spam posture ("a bot is publicly messageable
by anyone who finds its @username").
4. **Group addressing gate**: private chats always pass; group messages only pass if
they address the bot (`group_addressing`: @mention stripped from text, reply to the
bot's own message, or a `/cmd@botname` bot-command entity — verified with UTF-16
entity offsets).
5. **Per-conversation thread resolution**: `resolve_telegram_conversation_thread` —
conversation key = `chat_id`, via `ConversationRegistryStore::get_or_create` with a
lazy mint closure that builds a fresh `Thread` stamped with
`channel_origin = ChannelBridgeOrigin { kind, binding_id }`, emits `ThreadCreated`
on the EventBus, and marks the thread active in the `LeaseGate`. A private chat's
`chat_id` *is* the other user's id, so sender isolation falls out of the key.
6. **Delivery**: `submit_inbound_message` (shared) writes the transcript entry and
calls `QueueManagerRegistry::submit_message` — **the agent run is exactly a typed
chat turn**, tagged `MessageSource::Channel`. No channel-specific run path exists
anywhere.

### 1.5 The outbound pipeline (shared `relay/` infra + per-kind sink)

- **`CorrelationMap`** (`thread_id → reply target`): written by the inbound side just
before dispatch, read (never consumed) by the outbound observer; survives delegated
sub-runs; cleared only by `invalidate_thread` when a binding ends.
- **`relay/observer.rs::handle_relay_event`**: the shared state machine — buffer the
last `TextComplete` per thread since `RunStarted`; on `RunEnded`, check the
`LeaseGate`, resolve the correlation, hand the text to the kind's `RelaySink`.
`recover_lagged_replies` rebuilds a reply from the persisted transcript when the
broadcast channel reports `Lagged`; `terminal_failure_notice` converts non-`Completed`
end reasons into an honest user-facing notice instead of silence.
- **Per-kind `RelaySink`**: Telegram's `relay_reply` chunks via the shared
`relay/chunker.rs::chunk_text` (4096 chars), converts CommonMark → Telegram HTML
(`telegram/html.rs`), and sends. Telegram also runs a **typing heartbeat**
(`sendChatAction` every 4 s from `RunStarted` to `RunEnded`) as its kind-specific
extra around the shared skeleton.

### 1.6 Credentials, routes, frontend

- **Secrets**: `TelegramTokenStore` (crate `ao-engine-tools-provider-config`) — a thin
facade over `SecretVault` (OS keychain, else JSON vault under the data root), with a
deterministic env-var override (`LAUNCHPAD_TELEGRAM_TOKEN_<AGENT_ID>`) read first.
Same pattern exists for Discord/Slack secrets.
- **Routes** (`ao-server/src/routes/telegram.rs`, wired in `routes/mod.rs`):
`PUT/DELETE /agents/{id}/telegram/token` (validates via `getMe` before storing;
delete clears linked senders and invalidates relay state),
`GET .../telegram/status` (non-secret status for the setup modal),
`POST .../telegram/pairing-code`, `DELETE .../telegram/chats/{chat_id}`.
Enabling provisions the bridge thread via `channel_provisioning::provision_bridge_thread`.
Shared handlers in `routes/channels.rs`: `list_channels`, `get/set_channel_senders`,
and the email/discord/slack upsert+secret+delete triplets.
- **Frontend**: `frontend/src/lib/api.ts` has one client function per endpoint; the
agent editor (`components/chat/AgentProfileModal.tsx`) renders a per-kind settings
section (token field, bot username display, pairing-code generator, linked-chat list);
`ChannelsColumn`/`ChannelThreadList` surface channel threads in the Home sidebar.
- **Docs**: `guide/TELEGRAM.md` (BotFather → paste token → Save → `/start <code>`),
plus equivalent guide pages for the others.

### 1.7 The shape of "first-class"

Distilled, a first-class channel in this repo = **8 touch points**:

| # | Layer | Artifact |
| --- | --- | --- |
| 1 | protocol | `ChannelKind` variant + `ChannelKindConfig` variant + `ChannelCursor` variant |
| 2 | secrets | per-kind token store facade over `SecretVault` + env override |
| 3 | engine | `ChannelTransport` impl (client, inbound loop, pairing, addressing, thread resolve) |
| 4 | engine | `RelaySink` impl + outbound observer wiring (chunk, wire-format convert, typing) |
| 5 | engine | registration in `ChannelTransportRegistry` + any reconcile special-casing |
| 6 | server | per-kind routes (secret set/delete, status, pairing-code, unlink) + router wiring |
| 7 | frontend | api.ts functions + agent-editor settings section + channel thread display |
| 8 | docs | `guide/<KIND>.md` setup page |

Nothing in the plugin manifest (`plugin_manifest.rs`: `skills`, `rules`, `agents`,
`commands`, `hooks`, `mcp_servers`) can express any of these — `ChannelTransport` is
`pub(crate)` and `ChannelKind` is a closed enum. **A Matrix chat channel is a core
transport contribution, exactly like the four existing ones.** (An external Matrix bot
talking to Launchpad over the webhook assignment channel or an MCP connector is possible
but loses the conversational bridge-thread model entirely — see §3.7.)

---

## Part 2 — Mapping onto Matrix (cross-referenced with ecosystem prior art)

### 2.1 Concept mapping

| Launchpad/Telegram concept | Matrix equivalent | Notes |
| --- | --- | --- |
| Bot token | **Access token** for a bot user account (`@bot:server`), tied to a `device_id` | Obtained by login (`POST /login`) or created server-side (Synapse admin API / shared-secret registration). Stored identically in the vault. |
| `getMe` (token validation + `bot_username`) | `GET /_matrix/client/v3/account/whoami` → `user_id`, `device_id` | Same role: validate at save time, cache the bot's MXID in `kind_config`. |
| `getUpdates` long-poll + `offset` cursor | **`GET /sync` with `since` token** (`next_batch`) | Same shape, richer payload. The `/sync` long-poll-with-cursor pattern is the universal bot idiom — matrix-bot-sdk persists exactly this as its `syncToken` in bot storage. |
| `chat_id` (conversation key) | **`room_id`** (`!abc:server`) | Private chat ↔ DM room (`m.direct` account data); group ↔ multi-member room. Sender isolation needs `(room_id)` for DMs already — a Matrix DM room is per-counterparty by construction. |
| `/start <code>` pairing | **`!pair <code>`** message in any room/DM (configurable prefix) | Matrix has no `/start` deep-link; a command-prefix convention is the ecosystem norm (matrix-bot-sdk's `!bot hello`, baibot's chat commands). |
| Group addressing (@mention, reply, `/cmd@bot`) | **`m.mentions` containing the bot MXID; `m.in_reply_to` pointing at the bot's own event; `!bot`-style command prefix** | Matrix has first-class mentions (`m.mentions` content key) and rich replies — cleaner than Telegram's entity-offset math. |
| `sendMessage` with Telegram HTML | **`m.room.message` with `msgtype: m.text`/`m.notice`, `format: org.matrix.custom.html` + `formatted_body`** | Matrix's HTML subset is a superset of Telegram's; the existing `html.rs` converter concept transfers directly. |
| `sendChatAction("typing")` heartbeat | **`PUT /rooms/{roomId}/typing/{userId}`** (`{typing: true, timeout: ...}`) | Same heartbeat shape; Matrix typing notices have server-side expiry too. |
| 4096-char message cap | No comparable cap (event size limit is ~65 KB server-side) | Still chunk for readability — `chunk_text` with a generous limit (e.g. 4000) — and chunk *after* HTML conversion (the existing chunker's known pre-conversion limitation is worth not copying). |
| Update dedup after crash | Cursor = `next_batch` + a bounded `seen_event_ids` set | Mirror Discord's cursor variant shape. |
| (no equivalent) | **E2EE** — Matrix DMs are conventionally encrypted | The big Matrix-only concern; see §2.3. |
| (no equivalent) | **Room invites** — the bot must accept an invite before it can see a room | Auto-join-on-invite (matrix-bot-sdk's `AutojoinRoomsMixin` is the canonical pattern), gated by the pairing/allow-list policy. |
| (no equivalent) | **Homeserver URL** — Matrix is federated; the server is per-deployment config | Goes in `kind_config` (unlike Telegram's single hardcoded API base, which Telegram's client already makes overridable for tests). |

### 2.2 SDK choice: `matrix-sdk` (official Matrix.org Rust SDK)

The Rust choice is unambiguous and matches Launchpad's stack (async tokio, reqwest):

- `matrix-sdk` is the mid-level client library "ideal for building bots," maintained by
the Matrix.org Foundation, production-ready (backs Element X, Fractal, iamb).
- Basic bot shape maps 1:1 onto the existing transport: build a `Client`, log in or
restore a session, `add_event_handler`, and run `client.sync(SyncSettings)` — the
sync loop accepts a persisted token, which is the `ChannelCursor` slot.
- Feature flags: `e2e-encryption` (default), `sqlite`/`bundled-sqlite` (persistent
state + crypto stores), `markdown` (send Markdown-formatted messages — the agent's
reply is already CommonMark, so the outbound wire conversion can be nearly free).
- `bundled-sqlite` avoids a system-sqlite dependency — consistent with the repo's
preference for self-contained builds.

Prior art worth reading while implementing: **etkecc/baibot** (a mature Rust
matrix-sdk AI bot — context management, encryption, runtime chat commands),
**turt2live/matrix-bot-sdk** (TS; the sync-token persistence and auto-join patterns),
**matrix-org/matrix-rust-sdk examples**, and **matrixbot-ezlogin** (a small crate that
exists *purely* because bot E2EE bootstrap is fiddly — a useful reference, probably not
a dependency).

**Dependency-cost note:** matrix-sdk is heavy (~2 MSLoC with deps). Recommend a
separate workspace crate `ao-channel-matrix` (mirroring the `ao-engine-tools-*` split)
or at minimum a cargo feature gate so non-Matrix builds don't pay for it.

### 2.3 The E2EE decision (the one genuinely new problem)

Telegram/Discord/Slack bots read plaintext from a server API. A Matrix bot in an
encrypted room receives `m.room.encrypted` events it cannot read without crypto. DMs
on Matrix are conventionally E2EE, so this cannot be deferred silently:

- **matrix-sdk handles E2EE transparently** when set up: enable `e2e-encryption`,
persist state with the SQLite store, and **reuse the same `device_id` across
restarts** (the documented pitfall: a new device per login = previously-received
messages become undecryptable). Cross-signing bootstrap may need a one-time UIAA
password step.
- Device **verification is not required** for encryption to work (per matrix-sdk docs),
which keeps the bot UX tolerable: messages decrypt with an "unverified device"
warning at worst.
- **Recommended posture**: ship E2EE *on* from day one (transparent decrypt/encrypt via
matrix-sdk + `bundled-sqlite` crypto store under the agent's data-root channel dir,
store passphrase in `SecretVault`), document the device-verification caveat, and let
unencrypted rooms work trivially. Deferring E2EE means the bot is silently deaf in
exactly the DM rooms people will try first — the Telegram guide's "the one thing that
trips everyone up" lesson applied in advance.

---

## Part 3 — Implementation design (file by file)

### 3.1 Protocol (`crates/ao-protocol`)

```rust
// agent.rs
pub enum ChannelKind { Telegram, Discord, Email, Slack, WhatsApp, Webhook, Matrix }
// as_str(): ChannelKind::Matrix => "matrix"

pub enum ChannelKindConfig {
    // ...existing...
    Matrix {
        /// Cached from whoami at save time (mirrors Telegram's bot_username).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bot_user_id: Option<String>,
        /// Homeserver base URL, e.g. https://matrix-client.matrix.org
        homeserver_url: String,
        /// Accept room invites automatically once the inviting sender is
        /// linked (mirrors the autojoin pattern, gated by our allow-list).
        #[serde(default = "default_true")]
        auto_accept_invites: bool,
        /// In multi-member rooms, require an explicit address (mention/reply/
        /// command prefix). DMs are always addressed. Mirrors Telegram's
        /// group_addressing posture.
        #[serde(default = "default_true")]
        require_addressing_in_rooms: bool,
        /// Command prefix for pairing and any future chat commands.
        #[serde(default = "default_matrix_prefix")] // "!agent"
        command_prefix: String,
    },
}

// channel_cursor.rs
pub enum ChannelCursor {
    // ...existing...
    /// /sync's `next_batch` token: the server's own resume cursor — the
    /// Telegram `offset` analogue. `seen_event_ids` is a bounded FIFO
    /// (Discord-cursor-style) for belt-and-braces dedup across the
    /// crash-between-handle-and-persist window.
    Matrix {
        #[serde(default)] next_batch: Option<String>,
        #[serde(default)] seen_event_ids: Vec<String>,
    },
}
```

### 3.2 Secrets (`crates/ao-engine-tools-provider-config`)

`matrix_token_store.rs` — a byte-for-byte structural copy of
`telegram_token_store.rs`: facade over `SecretVault`, env override
`LAUNCHPAD_MATRIX_TOKEN_<AGENT_ID>` first, legacy-migration constants omitted (no
legacy). Two extra values ride along in the vault (same service, different account
keys): the **`device_id`** returned at login (reused on every restore — see §2.3) and
the **crypto-store passphrase** (generated once, vaulted). Access token itself can be
either user-pasted (advanced) or obtained via `POST /login` from username+password at
setup time (the friendly path; the password is never stored).

### 3.3 Engine transport (`crates/ao-engine/src/channels/matrix/`)

New module, deliberately mirroring `telegram/`'s file layout:

```javascript
channels/matrix/
  mod.rs        — module docs + re-exports (mirrors telegram/mod.rs)
  client.rs     — MatrixClient: thin wrapper over matrix_sdk::Client
                  (whoami, sync_once with since-token, send_message,
                  send_typing, join_room, leave_room)
  transport.rs  — ChannelTransport impl + run_sync_loop + handle_event
                  + try_link_room (!pair) + room_addressing (mention/reply/
                  command prefix) + resolve_matrix_conversation_thread
  outbound.rs   — run_outbound_observer + RelaySink<String> (room_id)
                  + typing heartbeat
  format.rs     — reply → Matrix formatted_body (leverage matrix-sdk's
                  `markdown` feature; fall back to plain body on conversion
                  failure, mirroring telegram/html.rs's escape-then-convert)
```

Key behaviors, each with its Telegram precedent:

- **`fingerprint`**: hash of `homeserver_url + resolved access token`. `None` when no
token → supervisor skips, exactly like a missing bot token today.
- **`run_sync_loop`**: restore `ChannelCursor::Matrix { next_batch, .. }`; loop
`sync_once(since)`; persist `next_batch` after each batch is handled; on error set
`ConnectionState::Reconnecting` + 5 s backoff (same constants). Re-read the agent
profile every iteration (the established mid-flight-config pattern).
- **Pairing**: message matching `<prefix> pair <code>` → `try_link_room` →
`LinkedSenderStore` write (keyed agent+binding, sender = room_id; plus the pairing
user's MXID recorded for display) → reply `m.notice` success/failure. Codes use the
existing `PairingCode::generate` (TTL, case-sensitive) — zero protocol change.
- **Allow-list**: reject-all default, `is_room_allowed` over `LinkedSenderStore` —
identical posture to `is_chat_allowed`, and identical guide-page warning.
- **Addressing** (`room_addressing`, analogue of `group_addressing`): DM rooms always
pass. Multi-member rooms pass iff `require_addressing_in_rooms` is off, or the event
(a) has the bot's MXID in `m.mentions.user_ids` (strip the mention from body), (b)
is `m.in_reply_to` one of the bot's own events (leave body intact), or (c) starts
with the command prefix (strip it). Skip own-sent events (`sender == bot_user_id`)
and non-`m.room.message` events at the top.
- **Invites**: on `m.room.member` invite for the bot: accept iff the inviter's MXID is
a linked sender (post-pairing world) — auto-accepting everything would let strangers
pull the bot into rooms, violating reject-all.
- **Thread resolution**: `resolve_matrix_conversation_thread(ctx, room_id, now)` —
identical body to Telegram's, conversation key = `room_id`. (Matrix *threads* via
`m.thread` relations are a real feature but defer to v2, same as Telegram forum
topics were deferred — record it as a known gap.)
- **Outbound**: `RelaySink<String>` sends chunked `m.room.message` (plain body +
`formatted_body`), `RelaySink` error mapping onto the shared `SendResult`/
`SendErrorKind` vocabulary (rate-limit → `RateLimited` with `retry_after` from
`M_LIMIT_EXCEEDED`). Typing heartbeat on `RunStarted` per correlated room,
`PUT .../typing/{bot}` every ~4 s, cancelled on `RunEnded` — same observer shape as
Telegram's.
- **Registration**: one line in `ChannelBridge::new` (or wherever the registry is
populated — mirroring `TelegramTransport`/`DiscordTransport`):
`registry.register(Arc::new(MatrixTransport::new(...)))`. The reconcile loop, leases,
`LeaseGate`, `ConnectionStateRegistry`, GC, and lagged-reply recovery need **no
changes** — they're all kind-generic. Add `ChannelKind::Matrix` to the
"mints threads on demand" placeholder arm in `reconcile` (the
`Discord | Telegram | Email` match).

### 3.4 Server routes (`crates/ao-server/src/routes/matrix.rs`)

Mirror `routes/telegram.rs` one-for-one, wired in `routes/mod.rs`:

| Endpoint | Handler notes |
| --- | --- |
| `PUT /agents/{id}/matrix/connection` | Body: `{ homeserver_url, access_token }` *or* `{ homeserver_url, username, password }`. Password path: `POST /login`, keep `access_token` + `device_id`, discard password. Validate via `whoami`, cache `bot_user_id` in `kind_config`, store token in vault, enable binding, provision bridge thread, save profile. |
| `DELETE .../matrix/connection` | Token + device out of vault, binding disabled, `kind_config.bot_user_id` cleared, `linked_senders.clear(agent, binding)`, `bridge.invalidate_thread(...)`, best-effort `POST /logout`. |
| `GET .../matrix/status` | `{ has_token, homeserver_url, bot_user_id, enabled, linked_room_ids, pending_pairing_code }` — never the token. |
| `POST .../matrix/pairing-code` | Identical to Telegram's (shared `PairingCode::generate`). |
| `DELETE .../matrix/rooms/{room_id}` | Unlink: store removal + `remove_if_matches` on the correlation map + profile display list. |

The shared `list_channels` / `get_channel_senders` / `set_channel_senders` handlers in
`routes/channels.rs` work unchanged once the kind exists.

### 3.5 Frontend

- `frontend/src/lib/api.ts`: `setMatrixConnection`, `deleteMatrixConnection`,
`getMatrixStatus`, `createMatrixPairingCode`, `unlinkMatrixRoom` — same shapes as the
telegram functions (the telegram test file `api.telegram.test.ts` is the template).
- `AgentProfileModal.tsx`: a Matrix section beside the others — homeserver URL,
token-or-password setup, bot MXID display, connection state chip (driven by the
existing `GET /agents/{id}/channels`), "Generate pairing code" button, linked-room
list with unlink. The generic channel-thread display (`ChannelsColumn`,
`ChannelThreadList`) needs, at most, a Matrix label/icon arm wherever `ChannelKind`
is rendered.
- Channel setup docs link out to `guide/MATRIX.md`.

### 3.6 Docs

`guide/MATRIX.md`, structured after `guide/TELEGRAM.md`: create the bot account
(Element registration or Synapse `register_new_matrix_user`), get an access token (or
use username+password in the setup modal), paste into the app, **Save**, DM the bot
`!agent pair <code>`, and the same bold warning up top: *empty allow-list rejects every
message — pair first.* Plus the E2EE/device-verification caveat section.

### 3.7 Alternatives considered (and rejected)

1. **Plugin (`plugin.json`)**: impossible by construction — plugins ship skills, rules,
agents, commands, hooks, and MCP servers; the channel seam is a closed enum +
`pub(crate)` trait. (A plugin could later ship Matrix-*using* skills to agents, but
not the channel itself.)
2. **External bridge bot** (standalone matrix bot → Launchpad webhook/MCP): zero core
changes, but no bridge threads, no transcript integration, no pairing model, no
shared lease/cursor/reply-recovery machinery — a worse product and a permanent
second-class citizen.
3. **Application Service / puppeting bridge**: designed for bridging whole platforms,
not bot-style interaction; massive complexity for no gain here.
4. **Defer E2EE**: rejected in §2.3 — the deaf-in-DMs failure mode is too likely and
too silent.

### 3.8 Testing strategy (all patterns already exist in-repo)

- **Cursor**: serde round-trip tests for the new variant (copy the existing four).
- **Transport**: the telegram `TestHarness` pattern — a mock homeserver via `wiremock`
(already a dev-dependency) serving canned `/sync`, `/whoami`, `/send` responses;
drive `handle_event` with synthetic events: pairing success/expired/wrong-case,
allow-list drop, DM pass-through, room mention strip, reply-to-bot pass,
unaddressed-room skip, invite accept/decline, per-room thread isolation, gc re-mint.
- **Outbound**: drive `handle_event` with synthetic `RunStarted`/`TextComplete`/
`RunEnded` (telegram's tests do exactly this), assert chunked sends in order and
heartbeat lifecycle.
- **Routes**: `crates/ao-server/tests/matrix_routes.rs` mirroring
`telegram_routes.rs` (529 lines of precedent).
- **Frontend**: `api.matrix.test.ts` mirroring `api.telegram.test.ts`.

### 3.9 Suggested PR slices

1. **Protocol + cursor + token store** (no behavior): enums, serde tests, store with
env override.
2. **Client + transport inbound** (sync loop, pairing, addressing, thread resolve) with
the mock-homeserver test harness.
3. **Outbound relay + typing + format**.
4. **Routes + provisioning wiring + frontend + guide page.**
5. **E2EE hardening pass**: crypto-store persistence validation, device-id reuse test,
cross-signing bootstrap docs (can ride with slice 2 if matrix-sdk setup is done
there from the start).

Estimated new code: ~3.5–4.5k lines of Rust (transport+outbound dominate, as with
Telegram's ~6k), ~400 lines of TypeScript, plus tests. Most of it is *pattern-following*,
not invention — the architecture was built for exactly this addition.
