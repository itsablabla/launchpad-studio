//! Inbound pipeline for the Matrix transport (guide/MATRIX_PLAN.md slice
//! A3): pairing, sender gating, addressing, dedup, and conversation→thread
//! resolution. The decision functions are pure and take plain data (per
//! D1's test layering); the two helpers that touch persistence
//! ([`try_link_room`], [`resolve_matrix_conversation_thread`]) mirror
//! Telegram's `try_link_chat` / `resolve_telegram_conversation_thread`
//! almost line-for-line.
//!
//! Gate order for one message event (see [`super::transport`]'s batch
//! handler):
//!
//! 1. **Dedup** — `seen_event_ids` from the cursor, checked first so a
//!    re-delivered batch never re-runs *any* side effect (not even a
//!    pairing reply).
//! 2. **Self-sent / notices / edits** — the loop guards. Own events are
//!    re-delivered by `/sync` (F5); notices are other bots' output; edits
//!    are dropped until B9's opt-in `process_edits`.
//! 3. **Pairing** — `<prefix> pair <code>` works from *unlinked* rooms;
//!    that's its purpose.
//! 4. **Allow-list** — reject-all default: the room must be a linked sender.
//! 5. **Addressing** — rooms require an explicit address (mention, reply to
//!    the bot, or command prefix); DMs always pass;
//!    `require_addressing_in_rooms: false` disables the room gate.
//!
//! Only then is a per-room thread resolved (minted on first contact) and the
//! message submitted — a dropped message never mints a thread it will never
//! use (same placement rule as Telegram's `handle_update`).

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use tracing::warn;

use ao_persistence::PersistenceLayer;
use ao_protocol::agent::AgentProfile;
use ao_protocol::conversation_registry::ConversationKey;
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::thread::{ChannelBridgeOrigin, Thread};

use crate::channels::relay::conversation_gc;
use crate::channels::ChannelRunContext;

use super::client::MatrixRoomEvent;

/// Replies sent to a room after a pairing attempt.
pub(crate) const PAIRING_SUCCESS_REPLY: &str =
    "Paired! This room is now linked to your Launchpad agent.";
pub(crate) const PAIRING_FAILURE_REPLY: &str =
    "That pairing code is invalid or expired. Generate a new one in Launchpad and try again.";

/// Parses the pairing command: exactly `<prefix> pair <code>` (three
/// tokens). Anything else — a bare prefix, extra arguments, ordinary
/// conversation — isn't a pairing attempt and falls through to the
/// allow-list gate. Case-sensitive on the code (the code is the secret),
/// exact-match on the prefix and verb.
pub(crate) fn parse_pair_command<'a>(text: &'a str, command_prefix: &str) -> Option<&'a str> {
    let mut parts = text.split_whitespace();
    if parts.next()? != command_prefix {
        return None;
    }
    if parts.next()? != "pair" {
        return None;
    }
    let code = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(code)
}

/// Why an event was dropped at the pre-gate — logged at debug by the caller.
/// Each variant is a loop hazard the design calls out (plan §4 "Loop
/// hazards", F5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropReason {
    /// Sent by the bot itself; `/sync` re-delivers own events.
    SelfSent,
    /// `m.notice` — bot convention: never respond to a notice, so two bots
    /// (or two bindings) can't loop each other.
    Notice,
    /// An `m.replace` edit. Inbound edits are B9's opt-in correction turns;
    /// delivering one as a fresh message would double-answer the prompt.
    Edit,
    /// Already in `seen_event_ids` (cursor-restored dedup).
    Duplicate,
}

/// The pre-gate: `Some(reason)` means drop before any other consideration.
/// Runs before pairing on purpose — a re-delivered pairing command must not
/// re-link (the code is already consumed, but the reply would re-send).
pub(crate) fn pre_gate(
    event: &MatrixRoomEvent,
    bot_user_id: &str,
    seen_event_ids: &[String],
) -> Option<DropReason> {
    if seen_event_ids.iter().any(|id| id == &event.event_id) {
        return Some(DropReason::Duplicate);
    }
    if event.sender == bot_user_id {
        return Some(DropReason::SelfSent);
    }
    if event.edited_event_id.is_some() {
        return Some(DropReason::Edit);
    }
    if event.is_notice {
        return Some(DropReason::Notice);
    }
    None
}

/// The addressing gate. Returns the text to dispatch — with the mention or
/// command prefix stripped — or `None` when the message doesn't address the
/// bot. DMs always pass unchanged. When `require_addressing_in_rooms` is off
/// every room message passes unchanged (the escape hatch for small trusted
/// rooms).
///
/// `reply_to_sender` is the sender of the replied-to event when it's one
/// we've seen this session (see [`RecentSenders`]); a reply to the bot
/// passes. A reply to an unknown/older event is treated as a reply to
/// someone else — worst case after a restart, a user replying to an ancient
/// bot message has to @-mention instead, which fails safe.
pub(crate) fn apply_addressing(
    body: &str,
    is_direct: bool,
    mentioned_user_ids: &[String],
    reply_to_sender: Option<&str>,
    bot_user_id: &str,
    require_addressing_in_rooms: bool,
    command_prefix: &str,
) -> Option<String> {
    if is_direct || !require_addressing_in_rooms {
        return Some(body.to_string());
    }
    if mentioned_user_ids.iter().any(|user| user == bot_user_id) {
        // Strip the literal MXID from the body; clients place it there as
        // the mention pill's plaintext fallback. Display-name pills don't
        // strip cleanly — harmless residue, documented in the guide.
        let stripped = body.replace(bot_user_id, "");
        return Some(stripped.trim().to_string());
    }
    if reply_to_sender == Some(bot_user_id) {
        return Some(body.to_string());
    }
    if let Some(rest) = body.trim_start().strip_prefix(command_prefix) {
        return Some(rest.trim().to_string());
    }
    None
}

/// Invite decision: auto-accept when the inviter is a linked sender, OR
/// while a live (unexpired) pairing code exists — the bootstrap path.
/// Without the code window, first contact deadlocks: pairing requires the
/// bot to be in the room, but an unlinked inviter's invite (which every
/// first invite is) would be rejected, so the bot could never get in to
/// hear the pair command. Presence is not authorization: a room the bot
/// joins through the window still can't reach the agent until it pairs —
/// the allow-list gate is unchanged — so a stranger who snipes the
/// 30-minute window gains a silent bot in their room, nothing more.
/// Blanket autojoin (no code required) would let strangers pull the bot
/// into rooms at any time with no intent signal at all; rejecting linked
/// users' invites would make room setup need manual client-side accepts
/// forever.
pub(crate) fn should_accept_invite(
    auto_accept_invites: bool,
    inviter_linked: bool,
    pending_code_live: bool,
) -> bool {
    auto_accept_invites && (inviter_linked || pending_code_live)
}

/// Process-local map of recently seen `event_id → sender MXID`, feeding the
/// reply-to-bot addressing check. Bounded FIFO; a restart loses it, which
/// fails safe (see [`apply_addressing`]).
pub(crate) struct RecentSenders {
    map: HashMap<String, String>,
    order: VecDeque<String>,
    cap: usize,
}

impl RecentSenders {
    pub(crate) fn new(cap: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), cap }
    }

    pub(crate) fn record(&mut self, event_id: &str, sender: &str) {
        if self.map.contains_key(event_id) {
            return;
        }
        while self.order.len() >= self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.map.insert(event_id.to_string(), sender.to_string());
        self.order.push_back(event_id.to_string());
    }

    pub(crate) fn sender_of(&self, event_id: &str) -> Option<&str> {
        self.map.get(event_id).map(String::as_str)
    }
}

/// Resolves a `<prefix> pair <code>` attempt against the binding's pending
/// pairing code at `now_unix`. On a match (present, unexpired, exact
/// case-sensitive match): links **both** the room (the allow-list gate keys
/// on rooms) and the pairing user's MXID (the invite gate keys on inviters —
/// [`should_accept_invite`]) into `LinkedSenderStore`, deduped, and clears
/// the code in memory. Mirrors Telegram's `try_link_chat`, including the
/// deliberate avoidance of `persistence.agents.update` (whole-document
/// writes clobber concurrent `PUT /agents/{id}` edits).
///
/// Returns whether linking happened so the caller picks the confirmation or
/// failure reply.
pub(crate) async fn try_link_room(
    persistence: &PersistenceLayer,
    profile: &mut AgentProfile,
    binding_id: &str,
    room_id: &str,
    pairing_user_mxid: &str,
    code: &str,
    now_unix: i64,
) -> Result<bool, AoError> {
    let Some(binding) = profile.channels.iter_mut().find(|b| b.binding_id == binding_id) else {
        return Ok(false);
    };

    let matches = binding
        .pending_pairing_code
        .as_ref()
        .is_some_and(|pending| !pending.is_expired(now_unix) && pending.code == code);
    if !matches {
        return Ok(false);
    }

    binding.pending_pairing_code = None;

    persistence.linked_senders.add_sender(&profile.id, binding_id, room_id).await?;
    persistence
        .linked_senders
        .add_sender(&profile.id, binding_id, pairing_user_mxid)
        .await?;
    Ok(true)
}

/// Resolves the conversation→thread registry row for a Matrix `room_id`,
/// lazily minting a fresh Launchpad bridge thread on first contact — the
/// Matrix analogue of `resolve_telegram_conversation_thread`, keyed on the
/// room id (rooms are the Matrix conversation boundary; native `m.thread`
/// sub-conversations are B9). Runs the gc-and-release pass first, exactly
/// like Telegram's, so an idle-evicted conversation re-mints cleanly.
/// Returns `None` (logged) only on a persistence failure — a message this
/// task cannot resolve a thread for is dropped rather than mis-routed.
pub(crate) async fn resolve_matrix_conversation_thread(
    ctx: &ChannelRunContext,
    room_id: &str,
    now: DateTime<Utc>,
) -> Option<String> {
    if let Err(e) = conversation_gc::run_gc_and_release_leases(
        &ctx.persistence.conversation_registry,
        &ctx.lease_gate,
        &ctx.agent_id,
        &ctx.binding_id,
        now,
    )
    .await
    {
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: conversation registry gc failed: {e}");
    }

    let key = ConversationKey::new(room_id.to_string());
    let mut minted_thread: Option<Thread> = None;
    let mint = || {
        let mut thread = ctx.persistence.threads.build_fresh_thread(&ctx.agent_id, None);
        thread.channel_origin =
            Some(ChannelBridgeOrigin { kind: ao_protocol::agent::ChannelKind::Matrix, binding_id: ctx.binding_id.clone() });
        let id = thread.id.clone();
        minted_thread = Some(thread);
        id
    };

    let row = match ctx
        .persistence
        .conversation_registry
        .get_or_create(&ctx.agent_id, &ctx.binding_id, key, now, mint)
        .await
    {
        Ok(row) => row,
        Err(e) => {
            warn!(agent_id = %ctx.agent_id, "MatrixTransport: failed to read the conversation registry: {e}");
            return None;
        }
    };

    if let Some(thread) = minted_thread {
        if let Err(e) = ctx.persistence.threads.create(thread.clone()).await {
            warn!(agent_id = %ctx.agent_id, "MatrixTransport: failed to create a per-conversation bridge thread: {e}");
            return None;
        }
        ctx.event_bus
            .emit(
                &format!("thread:{}", thread.id),
                &ctx.agent_id,
                Some(thread.id.clone()),
                AgentEventPayload::ThreadCreated { thread },
            )
            .await;
    }

    // Registered on every resolve, not just on first mint: `LeaseGate` is
    // process-local, so a conversation created by an earlier holder is
    // otherwise unknown to this process's gate (mirrors the Telegram/Discord
    // resolvers' reasoning).
    ctx.lease_gate.mark_active(&ctx.binding_id, &row.thread_id);
    Some(row.thread_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(body: &str) -> MatrixRoomEvent {
        MatrixRoomEvent {
            event_id: "$evt".to_string(),
            sender: "@alice:example.com".to_string(),
            body: body.to_string(),
            is_notice: false,
            mentioned_user_ids: Vec::new(),
            reply_to_event_id: None,
            edited_event_id: None,
        }
    }

    const BOT: &str = "@bot:example.com";

    // --- parse_pair_command ---

    #[test]
    fn pair_command_extracts_code_from_exact_shape() {
        assert_eq!(parse_pair_command("!agent pair ABC123", "!agent"), Some("ABC123"));
        assert_eq!(parse_pair_command("  !agent   pair   abc  ", "!agent"), Some("abc"));
    }

    #[test]
    fn pair_command_rejects_other_shapes() {
        assert_eq!(parse_pair_command("!agent pair", "!agent"), None);
        assert_eq!(parse_pair_command("!agent pair A B", "!agent"), None);
        assert_eq!(parse_pair_command("!agentPAIR A", "!agent"), None);
        assert_eq!(parse_pair_command("!other pair A", "!agent"), None);
        assert_eq!(parse_pair_command("!agent pair A", "!ops"), None);
        assert_eq!(parse_pair_command("pair A", "!agent"), None);
    }

    // --- pre_gate ---

    #[test]
    fn pre_gate_passes_a_normal_message() {
        assert_eq!(pre_gate(&event("hi"), BOT, &[]), None);
    }

    #[test]
    fn pre_gate_drops_self_sent() {
        let mut e = event("my own message");
        e.sender = BOT.to_string();
        assert_eq!(pre_gate(&e, BOT, &[]), Some(DropReason::SelfSent));
    }

    #[test]
    fn pre_gate_drops_notices() {
        let mut e = event("automated");
        e.is_notice = true;
        assert_eq!(pre_gate(&e, BOT, &[]), Some(DropReason::Notice));
    }

    #[test]
    fn pre_gate_drops_edits() {
        let mut e = event("* corrected");
        e.edited_event_id = Some("$orig".to_string());
        assert_eq!(pre_gate(&e, BOT, &[]), Some(DropReason::Edit));
    }

    #[test]
    fn pre_gate_drops_duplicates_before_anything_else() {
        // A duplicate self-sent edit reports Duplicate — the seen check
        // runs first so no gate side effect can re-fire.
        let mut e = event("hi");
        e.sender = BOT.to_string();
        e.edited_event_id = Some("$x".to_string());
        assert_eq!(pre_gate(&e, BOT, &["$evt".to_string()]), Some(DropReason::Duplicate));
    }

    // --- apply_addressing ---

    #[test]
    fn dm_always_passes() {
        assert_eq!(apply_addressing("hi", true, &[], None, BOT, true, "!agent").as_deref(), Some("hi"));
    }

    #[test]
    fn room_passes_everything_when_gate_disabled() {
        assert_eq!(
            apply_addressing("unaddressed", false, &[], None, BOT, false, "!agent").as_deref(),
            Some("unaddressed")
        );
    }

    #[test]
    fn room_mention_strips_the_mxid() {
        let mentions = vec![BOT.to_string()];
        assert_eq!(
            apply_addressing("hey @bot:example.com ping", false, &mentions, None, BOT, true, "!agent")
                .as_deref(),
            Some("hey  ping".trim())
        );
    }

    #[test]
    fn room_reply_to_bot_passes() {
        assert_eq!(
            apply_addressing("thanks", false, &[], Some(BOT), BOT, true, "!agent").as_deref(),
            Some("thanks")
        );
    }

    #[test]
    fn room_reply_to_someone_else_does_not_pass() {
        assert_eq!(
            apply_addressing("thanks", false, &[], Some("@alice:example.com"), BOT, true, "!agent"),
            None
        );
    }

    #[test]
    fn room_command_prefix_strips() {
        assert_eq!(
            apply_addressing("!agent status please", false, &[], None, BOT, true, "!agent")
                .as_deref(),
            Some("status please")
        );
    }

    #[test]
    fn unaddressed_room_message_is_skipped() {
        assert_eq!(apply_addressing("just chatting", false, &[], None, BOT, true, "!agent"), None);
    }

    // --- should_accept_invite ---

    #[test]
    fn invite_accepted_for_linked_inviter_with_auto_accept_on() {
        assert!(should_accept_invite(true, true, false));
        assert!(should_accept_invite(true, true, true), "a live code changes nothing for a linked inviter");
        assert!(!should_accept_invite(false, true, false));
    }

    #[test]
    fn invite_from_unlinked_inviter_accepted_only_while_a_pairing_code_is_live() {
        // The bootstrap path: a live code signals an operator is mid-pairing,
        // so the bot may join to hear the pair command. No live code, no join.
        assert!(should_accept_invite(true, false, true));
        assert!(!should_accept_invite(true, false, false));
        assert!(!should_accept_invite(false, false, true), "auto-accept off wins over the code window");
    }

    // --- RecentSenders ---

    #[test]
    fn recent_senders_recall_and_evict_in_order() {
        let mut senders = RecentSenders::new(2);
        senders.record("$a", "@alice:example.com");
        senders.record("$b", BOT);
        assert_eq!(senders.sender_of("$a"), Some("@alice:example.com"));
        senders.record("$c", "@carol:example.com");
        assert_eq!(senders.sender_of("$a"), None, "oldest evicted at cap");
        assert_eq!(senders.sender_of("$b"), Some(BOT));
        // Re-recording an existing id is a no-op (no double-order entry).
        senders.record("$b", BOT);
        assert_eq!(senders.map.len(), 2);
    }

    // --- try_link_room (persistence-level, mirrors Telegram's try_link_chat tests) ---

    use std::collections::HashMap;

    use ao_persistence::PersistenceLayer;
    use ao_protocol::agent::{
        AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig, CliProviderConfig, InputMode,
        MatrixEngagementConfig, MatrixStreamEdits, MatrixThreadMode, OutputFormat, PairingCode,
        ProviderConfig,
    };

    async fn make_persistence() -> (PersistenceLayer, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let layer = PersistenceLayer::init_with_root(ao_persistence::paths::DataRoot::new(
            tmp.path().to_path_buf(),
        ))
        .await
        .expect("init persistence");
        (layer, tmp)
    }

    fn matrix_binding_with_code(code: &str, expires_at_unix: i64) -> ChannelBinding {
        ChannelBinding {
            binding_id: "matrix".to_string(),
            kind: ChannelKind::Matrix,
            enabled: true,
            bridge_thread_id: None,
            allowed_senders: vec![],
            pending_pairing_code: Some(PairingCode { code: code.to_string(), expires_at_unix }),
            kind_config: ChannelKindConfig::Matrix {
                bot_user_id: Some(BOT.to_string()),
                homeserver_url: "https://matrix.example.com".to_string(),
                auto_accept_invites: true,
                require_addressing_in_rooms: true,
                command_prefix: "!agent".to_string(),
                thread_mode: MatrixThreadMode::Follow,
                stream_edits: MatrixStreamEdits::DmsOnly,
                process_edits: false,
                ack_reactions: true,
                feedback_reactions: true,
                send_read_receipts: true,
                download_attachments: true,
                download_max_bytes: 25 * 1024 * 1024,
                bot_display_name: None,
                engagement: MatrixEngagementConfig::default(),
            },
        }
    }

    fn make_agent(id: &str, binding: ChannelBinding) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 2,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            minimal_prompt: None,
            max_delegation_depth: None,
            channels: vec![binding],
            max_turns: None,
        }
    }

    #[tokio::test]
    async fn try_link_room_links_room_and_inviter_and_clears_code() {
        let (persistence, _tmp) = make_persistence().await;
        let mut agent = make_agent("agent-m", matrix_binding_with_code("ABC123", 1_700_000_600));

        let linked = try_link_room(
            &persistence,
            &mut agent,
            "matrix",
            "!room:example.com",
            "@alice:example.com",
            "ABC123",
            1_700_000_000,
        )
        .await
        .unwrap();

        assert!(linked);
        assert!(
            agent.channels[0].pending_pairing_code.is_none(),
            "in-memory code must be cleared for this batch"
        );
        let senders = persistence
            .linked_senders
            .get("agent-m", "matrix")
            .await
            .unwrap()
            .expect("store populated")
            .senders;
        assert!(
            senders.iter().any(|s| s == "!room:example.com"),
            "the room is linked for the allow-list gate: {senders:?}"
        );
        assert!(
            senders.iter().any(|s| s == "@alice:example.com"),
            "the pairing user is linked for the invite gate: {senders:?}"
        );
    }

    #[tokio::test]
    async fn try_link_room_rejects_expired_and_case_mismatched_codes_without_mutating() {
        let (persistence, _tmp) = make_persistence().await;
        let mut agent = make_agent("agent-m", matrix_binding_with_code("ABC123", 1_700_000_000));

        // now_unix == expires_at_unix: expired at the exact expiry instant.
        let linked = try_link_room(
            &persistence,
            &mut agent,
            "matrix",
            "!room:example.com",
            "@alice:example.com",
            "ABC123",
            1_700_000_000,
        )
        .await
        .unwrap();
        assert!(!linked);

        // The code is a secret: case-sensitive.
        let linked = try_link_room(
            &persistence,
            &mut agent,
            "matrix",
            "!room:example.com",
            "@alice:example.com",
            "abc123",
            1_699_999_000,
        )
        .await
        .unwrap();
        assert!(!linked);

        assert!(
            agent.channels[0].pending_pairing_code.is_some(),
            "a failed attempt keeps the code"
        );
        assert!(
            persistence.linked_senders.get("agent-m", "matrix").await.unwrap().is_none(),
            "a failed attempt links nothing"
        );
    }

    #[tokio::test]
    async fn try_link_room_dedupes_an_already_linked_room() {
        let (persistence, _tmp) = make_persistence().await;
        persistence
            .linked_senders
            .add_sender("agent-m", "matrix", "!room:example.com")
            .await
            .unwrap();
        let mut agent = make_agent("agent-m", matrix_binding_with_code("ABC123", 1_700_000_600));

        let linked = try_link_room(
            &persistence,
            &mut agent,
            "matrix",
            "!room:example.com",
            "@alice:example.com",
            "ABC123",
            1_700_000_000,
        )
        .await
        .unwrap();

        assert!(linked);
        let senders = persistence
            .linked_senders
            .get("agent-m", "matrix")
            .await
            .unwrap()
            .expect("store populated")
            .senders;
        assert_eq!(
            senders.iter().filter(|s| s.as_str() == "!room:example.com").count(),
            1,
            "re-pairing must not duplicate the room entry"
        );
    }
}
