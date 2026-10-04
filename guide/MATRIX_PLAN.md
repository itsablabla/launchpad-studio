# Matrix Channel — Execution Plan (v2 design, grounded)

Source designs: the v1 doc (`guide/matrix-channel-design.md`) and the v2 doc
("Feature-Rich, Emoji-Aware, Hardened"). This plan maps them onto the codebase as it
actually exists on 2026-10-03, corrects premise errors in the v2 doc, reconciles the
two docs where they differ, and sequences the work into landable slices.

## 0. State of play — the v2 doc's premises vs. the repo

The v2 doc assumes "v1 slices 1–5 landed". They have not:

- **No Matrix code exists.** No `ChannelKind::Matrix` (`ao-protocol/src/agent.rs:201`),
  no `ChannelKindConfig::Matrix` (`agent.rs:305`), no `ChannelCursor::Matrix`
  (`channel_cursor.rs:26`), no matrix module under `ao-engine`, no
  `matrix-sdk` dependency anywhere. This plan therefore includes the v1 baseline
  as Phase A, mapped onto the v1 doc's own five slices (its §3.9).
- **`Thread` has no `emoji` field.** The v2 doc's F4 claims one exists and is
  "free value nobody claimed". It doesn't — `emoji` lives on `AgentProfile`
  (`agent.rs:40`), `Project`, and `Workspace`, not on `Thread`
  (`ao-protocol/src/thread.rs:93`). F4 requires a protocol addition (Rust struct
  + TS `Thread` interface at `frontend/src/types/api.ts:1013` + a render arm that
  does not exist anywhere today).
- **The house pattern is hand-rolled transport clients.** Telegram = reqwest
  (`telegram/client.rs`), Discord/Slack = tokio-tungstenite + reqwest seams, all
  rustls. The v2 doc implicitly assumes matrix-sdk. See Decision D1 — this is the
  one place we deliberately break the pattern.

What the doc got right (verified): no channel handles reactions (Slack drops
`reaction_added`), no channel moves attachments (`build_channel_queued_message`
hardcodes `attachments: vec![]` at `channels/mod.rs:261` despite the full
`Attachment` protocol type at `ao-protocol/src/attachment.rs:19` and the runner's
`augment_prompt_with_attachments` at `agent_runner/shared.rs:140`), Discord's
engagement machine (`channels/discord/engagement.rs:173`) and backfill
(`backfill.rs:251`) are the right base, and the observer's
`terminal_failure_notice` arm (`channels/relay/observer.rs:239`) is exactly where
F1's ⚠️ wires in.

## 1. Decisions

### D1 — matrix-sdk, not a hand-rolled client (the one pattern break)

Telegram/Discord/Slack are hand-rolled because their APIs are simple HTTP/WS.
Matrix without E2EE is likewise simple — but the design requires E2EE done right
(key backup, UISI handling, device discipline), and hand-rolling megolm/olm is not
a responsible option. Decision:

- `matrix-sdk` with features `bundled-sqlite`, `e2e-encryption`, `rustls-tls`,
  `markdown` (rustls to match the workspace TLS stack; `markdown` per v1 §2.2
  makes the outbound wire conversion nearly free — the agent's reply is already
  CommonMark).
- **Where it lives — cargo feature gate, not a separate crate.** The v1 doc
  (§2.2) suggests a separate `ao-channel-matrix` crate; that doesn't survive
  contact with the codebase: `ChannelTransport`/`ChannelTransportRegistry` are
  `pub(crate)` (`channels/mod.rs:98,160`), so an external crate cannot implement
  or register a transport without making the whole seam public API. Instead:
  the module lives inside `ao-engine` behind a `matrix` cargo feature (default
  on). `--no-default-features` builds skip the ~2 MSLoC of matrix-sdk deps;
  making the seam public stays off the table.
- **Cost acknowledged**: matrix-sdk is the heaviest dependency the workspace will
  have (vodozemac, bundled sqlite, ~3-5 min added cold compile). Acceptable for a
  desktop app; CI cache needs to cover it.
- **Crypto store from day one, backup automation later**: the sqlite crypto store
  path is fixed at `ClientBuilder` time and painful to retrofit onto an existing
  device. So Phase A builds the client with `e2e-encryption` + persistent store
  (pure config; this is v1 slice 5 "riding with slice 2" as v1 §3.9 allows),
  while `Recovery::enable`/`recover_and_fix_backup` lands in slice B7 as designed.
- **Testability, two layers** (reconciling v1 §3.8 with the seam precedent):
  (a) pure handler functions over already-deserialized events — pairing,
  addressing, gates, dedup — tested directly, no HTTP at all; (b) the thin
  `MatrixClient` adapter is tested against a **wiremock mock homeserver**
  (already a dev-dependency, per v1 §3.8) serving canned `/sync`, `/whoami`,
  `/send` — the real SDK client talks real HTTP to the mock, so the adapter
  needs no fake. No Discord-style hand-rolled seam trait is needed.

### D2 — `Thread.emoji` protocol addition

Add `emoji: Option<String>` (serde-defaulted) to `Thread`, mirror in the TS
interface, and add one render arm in `ThreadTabStrip.tsx` / `ChannelThreadList.tsx`
(emoji wins over the per-kind icon when set). Small, additive, no migration. F4
depends on it.

### D3 — Module placement

`ao-engine/src/channels/matrix/` — where Discord/Slack/Email live, and where the
v1 doc puts it (its §3.3). (`telegram/` is the top-level historical exception only
because `ChannelBridge` grew up there; newer channels all live under `channels/`.)
Layout per v1 §3.3, plus the two v2 additions: `client.rs` (SDK adapter),
`transport.rs` (sync loop + inbound), `outbound.rs` (relay + reactions +
streaming), `format.rs`, `security.rs` (v2: sanitize/MIME/authz rules), `e2ee.rs`
(v2: recovery automation), `mod.rs`.

### D4 — Process-lock pairing

`ChannelLeaseStore` (`ao-persistence/src/channel_lease_store.rs:90`) claims with an
`owner_id`; matrix-sdk's `cross_process_store_locks_holder_name` takes a holder
name. Pass the **same owner_id string** to both so "who owns this binding" has one
answer at both layers (design §3.6).

### D5 — Secrets: `ChannelSecretStore`, not a legacy-shaped token store

The v1 doc (§3.2) proposes a byte-copy of `telegram_token_store.rs`. We deviate:
`TelegramTokenStore` is the legacy single-purpose facade; Discord/Slack use the
newer role-based `ChannelSecretStore` (`channel_secret_store.rs:152`). Matrix gets
three roles: `MATRIX_TOKEN_SECRET_ROLE = "matrix_token"`,
`"matrix_device_id"` (vaulted at login, always restored with the token — v1 §2.3's
device-id discipline), and `"matrix_store_passphrase"` (generated once, for the
crypto store). Same `SecretVault`/env-override mechanics
(`LAUNCHPAD_CHANNEL_SECRET__<AGENT>__<BINDING>__<ROLE>` already works for free).

## 2. Slice plan

Phase A = the v1 baseline (never landed; slices map onto v1 §3.9's five slices).
Phase B = v2 slices 6–9. Slice exit criteria = the v1 doc's "8 touch points"
checklist (its §1.7) for whatever subset the slice owns. Every slice is
independently shippable, test-gated (`cargo test --workspace` green + targeted
frontend tests), and lands behind the controlled stop→edit→test→relaunch ritual
for anything `crates/`-touching.

### A1 — Protocol & plumbing surface *(v1 slice 1)*

- `ChannelKind::Matrix` + `as_str` arm; `ChannelKindConfig::Matrix` with the v1
  fields (v1 §3.1) **and all v2 §4 fields now** (serde-defaulted, no migration
  cost, avoids a second enum churn later).
- `ChannelCursor::Matrix { next_batch: Option<String>, seen_event_ids: Vec<String> }`
  (bounded FIFO shape mirrors Slack's; serde round-trip tests copied from the
  existing four).
- Secret roles per D5 (token + device id + store passphrase).
- Routes land in A4, not here — v1 slice 1 is no-behavior, and the connection
  route needs the client from A2 to validate via `whoami`.

### A2 — Client bootstrap & sync loop *(v1 slice 2, absorbing v1 slice 5's store setup)*

- `MatrixClient` adapter over `matrix_sdk::Client` (whoami, sync_once with
  since-token, send_message, send_typing, join/leave) with `device_id` +
  store passphrase vaulted per D5; `restore_session` only ever with the pair.
- Persistent stores at `<data_root>/agents/{id}/matrix/`; cross-process lock
  holder = lease owner_id (D4); `e2e-encryption` + `bundled-sqlite` on from the
  first build (D1).
- Sync loop per v1 §3.3: restore cursor, `sync_once(since)` loop, persist
  `next_batch` only after batch handled, error taxonomy →
  `ConnectionStateRegistry` (unknown-token → Disconnected; unreachable →
  jittered backoff + Reconnecting), profile re-read every iteration.
- `ChannelTransport` impl (`fingerprint` = hash of homeserver_url + token,
  `None` without token) + registration at `telegram/bridge.rs:143`, plus the
  `ChannelKind::Matrix` arm in `reconcile`'s mints-on-demand match (v1 §3.3).
- Tests: wiremock mock homeserver (v1 §3.8) — cursor restore, taxonomy mapping,
  device-id reuse across "restart" (v1 slice 5's test).

### A3 — Inbound pipeline *(rest of v1 slice 2)*

- Pairing: `<prefix> pair <code>` → `LinkedSenderStore` write (sender = room_id,
  pairing user's MXID recorded for display) + `m.notice` reply — v1 §3.3, using
  the shared `PairingCode::generate` (zero protocol change).
- Allow-list gate: reject-all default over `LinkedSenderStore` (Telegram's
  `is_chat_allowed` posture, `transport.rs:617`).
- **Invites** (from v1 §3.3; my earlier draft missed these): on `m.room.member`
  invite for the bot, auto-accept iff the inviter is a linked sender — blanket
  autojoin would let strangers pull the bot into rooms and break reject-all.
- Addressing: `m.mentions` contains bot MXID (strip), `m.in_reply_to` a bot event
  (pass), or command prefix (strip); DMs always pass; `require_addressing_in_rooms`
  escape hatch. Engagement machine NOT yet (that's B9).
- Conversation key `(room_id)` → thread mint via
  `threads.build_fresh_thread` + `ChannelBridgeOrigin` stamp + `ThreadCreated`
  emit (exactly `transport.rs:540-601`'s flow), then `submit_inbound_message`
  (`channels/mod.rs:319`).
- Dedup: `seen_event_ids` checked before any gate; redacted events (empty content)
  ignored; own-sent events ignored (the F5 loop guard exists from day one).
- Tests (pure handlers, D1 layer a): pairing success/expired/wrong-case,
  allow-list drop, DM pass, room mention strip, reply-to-bot pass, unaddressed
  skip, invite accept/decline, per-room thread isolation, dedup across "restart"
  (cursor reload), redaction drop, gc re-mint.

### A4 — Outbound relay + routes *(v1 slices 3 + 4's backend half)*

- `RelaySink<MatrixReplyTarget>` impl; correlation map value is
  `{ room_id, trigger_event_id }` **from the start** (v2's widening is then free).
- `format.rs`: markdown → spec-HTML allow-list subset (matrix-sdk `markdown`
  feature, plain-body fallback); chunker operates post-conversion (v2's fix
  folded into the first version instead of inheriting the known limitation).
- Typing heartbeat on `RunStarted`, cancelled on `RunEnded` (telegram
  `outbound.rs:254` pattern; `PUT /rooms/{id}/typing/{bot}`).
- Plain queued sends only — retry/ordering hardening is B6, ack reactions B8.
- Routes (`routes/matrix.rs`, wired in `routes/mod.rs`) per v1 §3.4:
  `PUT .../matrix/connection` accepting **either** `{ homeserver_url,
  access_token }` **or** `{ homeserver_url, username, password }` (password path
  logs in via the SDK, keeps token + device_id, discards the password — the
  friendly setup path), validated via `whoami`, `bot_user_id` cached in
  `kind_config`, binding enabled + bridge thread provisioned; `DELETE
  .../matrix/connection`; `GET .../matrix/status` (never the token);
  `POST .../matrix/pairing-code`; `DELETE .../matrix/rooms/{room_id}`.
- Tests: format/chunk tables, heartbeat lifecycle, relay through
  `handle_relay_event` incl. `terminal_failure_notice`;
  `ao-server/tests/matrix_routes.rs` mirroring `telegram_routes.rs` (529 lines
  of precedent, v1 §3.8).

### A5 — Frontend & docs *(v1 slice 4's frontend half + §3.6)*

- `MatrixTabPanel` in `AgentProfileModal.tsx` + `ChannelsTabPanel` arm (homeserver
  URL, token set/clear, status badge, pairing code), TS types in `lib/api.ts`,
  `CHANNEL_KIND_LABELS` + icon map entries (`threadNavigation.ts:19`,
  `ThreadTabStrip.tsx:111`).
- `guide/MATRIX.md`: homeserver setup, bot account (Element registration or
  Synapse `register_new_matrix_user`), token-or-password setup (password never
  stored), **Save**, `!agent pair <code>`, the bold reject-all warning up top,
  invite flow, E2EE/device-verification caveat section (v1 §3.6).
- Tests: panel renders/submits (mirror `TelegramTabPanel.test.tsx`).

### B6 — Reliability core *(v2 slice 6, unchanged)*

`send_with_retry` (Retry-After header first, `retry_after_ms` fallback, jittered
ceiling backoff → `SendErrorKind`), txn-id idempotent sends, per-room ordered mpsc
send queue, sync-filter tuning, connection-state detail strings, per-binding
counters, `GET /agents/{id}/matrix/doctor`.

### B7 — E2EE recovery automation *(v2 slice 7)*

`Recovery::enable` + vaulted passphrase after first login,
`recover_and_fix_backup()` each startup, verification + `BackupState` in status
route + frontend badges, UISI honest placeholder. SAS verification documented in
the guide.

### B8 — Rich content *(v2 slice 8 + D2)*

Ack reactions (👀/✅/⚠️ wired through the observer's existing arms — correlation
map already carries `trigger_event_id` since A4), inbound 👍/👎 feedback reactions,
custom-emote `<img data-mx-emoticon>` cleaning both ways (surrogate-pair test
ported from Telegram's UTF-16 test), room name/avatar → thread title +
**`Thread.emoji` (D2)**, full outbound HTML subset incl. spoilers.

### B9 — Interaction depth *(v2 slice 9)*

`m.replace` streaming (buffered TextDeltas, 1.5 s throttle, DMs default-on),
opt-in inbound edits as correction turns (Hermes rules: own-message only, bot-edits
never, deduped), attachments both ways (first channel to populate
`QueuedMessage.attachments`; download cap + MIME allow-list; outbound artifact
upload), read receipts + inbound typing surfacing, `m.thread` conversation keys
with MSC3440 fallback replies, Discord engagement machine + backfill port
(`EngagementTracker` is already channel-agnostic — `EngagementInput` needs only a
Matrix caller).

## 3. Validation strategy

- **Per slice**: its tests green + `cargo test --workspace` + `cargo check
  --workspace --all-targets`; frontend slices add `tsc` + vitest.
- **Two-layer tests** (D1): pure handler unit tests need no homeserver; adapter
  tests use the wiremock mock homeserver; `matrix_routes.rs` mirrors
  `telegram_routes.rs`; frontend mirrors `api.telegram.test.ts` (all v1 §3.8).
- **Live smoke per phase gate**: manual checklist against a throwaway homeserver
  (local `conduit`/`synapse` in docker, or a matrix.org test account): pair → DM →
  group mention → restart dedup → (B7+) encrypted room round-trip. Checklist lives
  in `guide/MATRIX.md`.
- **Relaunch ritual**: controlled stop → edit → test → relaunch for all
  `crates/` changes; keychain ACL re-prompt expected on every backend rebuild
  (observed 2026-10-03 — the rebuilt binary re-prompts for stored channel
  secrets until "Always Allow").

## 4. Risks & mitigations

| Risk | Mitigation |
| --- | --- |
| matrix-sdk compile weight / version churn | pin minor version; `matrix` cargo feature gate (D1); CI cache; adapter isolation |
| Crypto store corruption from dual processes | D4 paired locks; doctor checks store readability |
| Losing encrypted history (OpenClaw's failure mode) | B7 makes backup structural, not optional; status badge warns when `BackupState` unhealthy |
| Rate-limit drops (matterbridge's bug) | B6 before any feature that multiplies egress (B8/B9 funnel through `send_with_retry`) |
| Loop hazards (own edits/reactions re-ingested) | sender==bot guard in A3, before B5/B8 add self-originated events |
| Attachment abuse | size cap + MIME allow-list in `kind_config`, enforced before download |

## 5. Suggested sequencing & rough sizing

A1 (S) → A2 (L, the SDK integration) → A3 (M) → A4 (M) → A5 (S) ⇒ **usable
baseline**. Then B6 (M) → B7 (M) ⇒ **production-stable**. Then B8 (M) →
B9 (L) ⇒ **feature-rich**. B8/B9 are independently reorderable; both depend on
B6's egress path. Total: 9 slices, each a clean PR. v1 §3.9 sizes Phase A at
~3.5–4.5k lines of Rust + ~400 of TypeScript plus tests; Phase B adds roughly
the same again, dominated by B9.

## 6. Reconciliation log (v1 × v2 × this plan)

Where the two design docs conflict or the repo disagreed with both, and what this
plan does about it:

| Topic | v1 says | v2 says | Plan |
| --- | --- | --- | --- |
| Matrix exists already | — | "v1 slices 1–5 landed" | They didn't; Phase A = v1 §3.9 slices |
| `Thread.emoji` | — | "exists, set to None everywhere" | It doesn't; D2 adds it (protocol + TS + render arm) |
| SDK packaging | separate `ao-channel-matrix` crate "or feature gate" | assumes matrix-sdk | feature gate inside `ao-engine` — the transport seam is `pub(crate)` (D1) |
| Secrets store | copy `telegram_token_store.rs` | — | `ChannelSecretStore` roles (D5) — the newer Discord/Slack pattern |
| Module path | `channels/matrix/` | same (artifact table) | `channels/matrix/` (D3) |
| Crypto store timing | slice 5 "can ride with slice 2" | "from day one" | day one (D1); backup automation stays in B7 |
| Correlation value | `RelaySink<String>` (room_id) | widen to `{room_id, trigger_event_id}` | wide from the start (A4) |
| Test seam | wiremock mock homeserver | implies SDK-level | both layers: pure handlers + wiremock adapter (D1) |
| Chunk timing | chunk after HTML conversion | same ("fixing, not copying") | post-conversion from the first version (A4) |
