# Matrix Channel — Setup & Usage

Connect a Launchpad agent to a Matrix bot account so you can DM the bot (or
@-mention it in a paired room), have the agent run against the message, and get
the reply back in the same conversation — a full round trip over any
Matrix homeserver (Synapse, Dendrite, Conduit, …).

> **The one thing that trips everyone up:** an agent with an **empty linked-room
> list rejects every message** (reject-all security model). You must **link the
> conversation first** (`!agent pair <code>`). A normal message sent before
> linking is silently dropped and will never appear in a thread.

---

## 1. Create a bot account (one time)

The bridge needs a **dedicated Matrix account** for the agent — a normal user
account you create on your homeserver, used only as the bot.

- On a public homeserver (e.g. matrix.org): register a new account in any
  client (Element → "Create account"), e.g. `@my-agent-bot:matrix.org`.
- On a self-hosted Synapse: `register_new_matrix_user -u my-agent-bot -p <password>`
  (or your server's admin API).

You need either the account's **username + password** (the app logs in once,
keeps the session token, and discards the password) or an **access token +
device id** from an existing session (advanced).

## 2. Connect the bot account to an agent

In the Launchpad app:

1. Open the agent → **Channels** tab → **Matrix** sub-tab.
2. Enter the **homeserver URL** (e.g. `https://matrix.example.com`).
3. Pick an auth mode:
   - **Username + password** (default): enter the bot account's credentials.
     The app performs one `m.login.password` login, vaults the returned access
     token + device id, and discards the password immediately.
   - **Access token**: paste a token directly. The token's session **must have
     a device id** — the bridge needs a stable device identity, and tokens from
     clients that log in without one (some SSO flows) are rejected with a
     message telling you so.
4. **Connect.** The app validates the credential against the homeserver
   (`GET /account/whoami` / login) *before* storing anything — a rejected
   credential is never vaulted. On success the binding is enabled and the
   bot's MXID is shown.

Once connected, the bridge starts a long-running `/sync` loop for the bot.

> The access token lives in the OS keychain (with the standard
> file fallback), never in the agent profile JSON. **Disconnect** deletes the
> token, disables the binding, and revokes every linked room and sender.

## 3. Linking conversations (DMs & rooms)

This is what puts a conversation on the agent's allow-list. Until a
conversation is linked, the bridge silently drops every message from it —
that's the expected anti-spam gate, not a bug.

> [!IMPORTANT]
> **Every conversation needs its own pairing code.** Each DM and each room is
> paired **separately**, using a **freshly generated code each time** — codes
> are **single-use** and **expire after 30 minutes**. Pairing a new room only
> *adds* to the allow-list; it never removes rooms you already paired.

### Pair a DM

1. In the agent's Matrix settings → **Generate pairing code**.
2. In your Matrix client, open a DM with the bot account and send:
   **`!agent pair <code>`** (e.g. `!agent pair 4F9K2A`).
3. The bot replies (as a notice):
   > *You're linked. I'll respond to messages in this conversation from now on.*

### Pair a room

1. Invite the bot account into the room. The invite is accepted automatically
   **if you have already paired with this agent** (your MXID is linked by your
   first pairing — the invite gate is "a linked user invited me"). Otherwise
   invite it and complete step 3 before the invite matters.
2. In the agent's Matrix settings → **Generate a new pairing code**.
3. In the room, send **`!agent pair <code>`**.
4. The bot confirms the room is linked.

Each paired room appears in the agent's **Linked rooms** list in settings.
Unlinking a room there also makes the bot **leave the room** (best-effort);
your own MXID stays linked, so future invites from you are still accepted.

## 4. Use it

- **DMs:** every message reaches the agent — no addressing needed.
- **Rooms:** the agent only listens when **addressed**:
  - @-mention the bot (`@my-agent-bot:…`), or
  - **reply** to one of the bot's messages, or
  - start the message with the command prefix: **`!agent`** (e.g.
    `!agent summarize this thread`).

The reply is relayed back into the same conversation. Markdown in the reply is
rendered as Matrix HTML; long replies are split into multiple messages
automatically. While the agent is working, the bot shows a typing indicator.

Every conversation (each DM, each room) gets its own **dedicated thread** in
the app, created automatically the first time a message arrives there.

---

## How it works (mechanism)

1. The **bridge supervisor** spawns one `/sync` task per Matrix-enabled agent,
   using the vaulted token + device id and a per-agent crypto store
   (`matrix.sqlite` under the agent's data dir; passphrase from the keychain).
2. Each sync batch is filtered:
   - Events already seen (dedup list, survives restarts), the bot's own
     messages, edits, and `m.notice` posts (bot-loop guard) are dropped first.
   - `!agent pair <valid code>` → the **room id and the sender's MXID** are
     appended to the allow-list. The MXID entry is what powers the invite gate.
   - **Empty allow-list ⇒ reject all.**
   - Room messages must additionally be *addressed* (mention / reply-to-bot /
     `!agent` prefix); DMs are always addressed by definition.
3. An accepted message is submitted to the agent's per-conversation **bridge
   thread** (one thread per room, minted on first contact).
4. On turn completion, the reply is relayed back through the same live
   client (correlated via an in-flight `thread_id → room` side-map), chunked
   to the homeserver's event-size budget and converted to spec-HTML.

The sync cursor is persisted after each handled batch, so a restart resumes
where it left off instead of replaying history; the initial sync's backlog is
never dispatched.

---

## Troubleshooting

### Nothing arrives at all — the bot isn't syncing

Check the **Matrix** tab's connection badge:

- **Disconnected** — no backend process is running the binding (or the token
  was revoked homeserver-side; reconnect with fresh credentials).
- **Reconnecting…** — the sync loop is backing off (homeserver unreachable).
- **Held by another process** — a second backend pointed at the same data
  directory owns this connection right now. Not an error; only one process may
  run a binding at a time.

### Messages sent, but they never reach a thread

You almost certainly **haven't linked the conversation**. Empty allow-list =
reject-all. Generate a pairing code and send `!agent pair <code>` in that
DM/room first (Section 3), *then* message.

### In a room, the bot answers some messages but ignores others

Working as intended: in rooms the bot only listens when addressed — @-mention
it, reply to one of its messages, or prefix with `!agent`. (DMs always reach
it.) If you want it to read everything in a room, that's a deliberate per-room
choice — currently a config default, not a UI toggle.

### The bot never joined the room I invited it to

Invites are auto-accepted in two cases: the inviter is a **linked user** (you've
paired with this agent before — any conversation counts), **or a pairing code
is currently live** (the 30-minute window after you generate one — that's the
bootstrap path, so generate the code *before* inviting the bot). Outside those,
the invite is ignored by design: presence without a pairing handshake grants
nothing anyway, since the allow-list still gates every message.

---

## Quick reference

| Step | Where | Action |
|---|---|---|
| Create bot account | Your homeserver | Register a normal user account for the bot |
| Connect | App → agent → Channels → Matrix | Homeserver URL + username/password (or token) → **Connect** |
| Pair *each* conversation | App → Matrix settings | **Generate a new pairing code** (single-use, 10 min expiry) |
| Link *each* conversation | Matrix (the DM or room) | `!agent pair <code>` → "You're linked." |
| Use (DM) | Matrix DM | Send a normal message |
| Use (room) | Matrix room | @-mention, reply to the bot, or prefix `!agent` |
| Unlink | App → Matrix settings | Remove that room from the linked list (bot leaves it) |
| Disconnect | App → Matrix settings | **Disconnect** — deletes token, revokes all links |
