//! Matrix `ChannelTransport`: one `/sync` long-poll loop per binding, fed by
//! the [`MatrixClient`] adapter.
//!
//! Slice A3 scope: the inbound pipeline. Each successful batch runs
//! [`handle_sync_batch`] — invites, then per event: dedup → self/notice/edit
//! guards → pairing → linked-room allow-list → addressing → per-room
//! conversation→thread resolution → `submit_inbound_message` — and only then
//! does the loop persist the cursor, so a crash mid-batch re-delivers at
//! most one batch (absorbed by `seen_event_ids`).
//!
//! Loop posture mirrors Telegram's `run_bot_poll_loop` (profile re-read every
//! iteration, jittered backoff on transient errors, terminal stop on a dead
//! token) with Matrix's differences folded in: the resume token is the
//! homeserver's `next_batch`, and session identity (token + device id) comes
//! from the `ChannelSecretStore` roles of D5.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::task::JoinHandle;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use ao_engine_tools_provider_config::channel_secret_store::ChannelSecretStore;
use ao_engine_tools_provider_config::{
    MATRIX_DEVICE_ID_SECRET_ROLE, MATRIX_STORE_PASSPHRASE_SECRET_ROLE, MATRIX_TOKEN_SECRET_ROLE,
};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig};
use ao_protocol::channel_connection_state::ChannelConnectionState;
use ao_protocol::channel_cursor::ChannelCursor;

use crate::channels::relay::correlation_map::CorrelationMap;
use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::{submit_inbound_message, ChannelRunContext, ChannelTransport};
use crate::event_bus::EventBus;

use super::client::{
    preflight_whoami, MatrixClient, MatrixClientError, MatrixClientErrorKind,
    MatrixConnectionParams, SyncBatch,
};
use super::inbound::{
    apply_addressing, parse_pair_command, pre_gate, resolve_matrix_conversation_thread,
    should_accept_invite, try_link_room, RecentSenders, PAIRING_FAILURE_REPLY,
    PAIRING_SUCCESS_REPLY,
};
use super::outbound::{self, MatrixReplyTarget};

/// Process-wide registry of live Matrix clients, keyed by
/// `(agent_id, binding_id)`. Each binding's sync task registers its client
/// on every (re)connect and unregisters on exit; the outbound relay and
/// typing heartbeat send through the registered client (never a fresh one —
/// `room.send` on the *synced* client encrypts automatically for E2EE
/// rooms, and reuses its store/session).
type ClientRegistry = Arc<StdMutex<HashMap<(String, String), Arc<MatrixClient>>>>;

/// Base pause after a failed sync/connect attempt, before jitter. Keeps one
/// unhealthy binding (revoked token, homeserver down) from hammering the
/// homeserver or spinning the task hot — same posture as Telegram's
/// `ERROR_BACKOFF`, jittered per the plan's error taxonomy (§A2).
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Upper bound on the in-memory dedup list mirrored into
/// `ChannelCursor::Matrix.seen_event_ids`. Sized like Slack's seen-ids list:
/// enough to cover a burst re-delivered across a restart, small enough to
/// round-trip through YAML without notice.
const SEEN_EVENT_IDS_CAP: usize = 512;

/// Matrix's [`ChannelTransport`] implementation. Builds no clients of its
/// own — a `MatrixClient` is built inside each binding's sync task (the
/// client is bound to that binding's token/device/store, so sharing across
/// bindings would be wrong) — but holds a registry of the clients those
/// tasks are currently running ([`ClientRegistry`]) so the outbound relay
/// and the HTTP routes can send through them. `pub` because ao-server's
/// matrix routes name it (`AppState::matrix_transport`); its surface beyond
/// the two route-facing methods stays crate-private.
pub struct MatrixTransport {
    /// Lazily-opened secret store (Discord's `OnceLock` pattern): opening
    /// touches the OS keychain, so it happens on first use, not at
    /// construction where a failure would take down `AppState::new`.
    secret_store: OnceLock<ChannelSecretStore>,
    /// Live clients per binding — see [`ClientRegistry`].
    clients: ClientRegistry,
    /// `thread_id -> MatrixReplyTarget` for turns the inbound pipeline just
    /// dispatched. Written in [`handle_sync_batch`] right before submit,
    /// read (never consumed) by the outbound observer at `RunStarted` /
    /// `RunEnded`, cleared by `invalidate_thread`/`invalidate_binding` —
    /// the exact lifecycle `TelegramTransport`'s `InFlightChats` documents.
    in_flight: Arc<CorrelationMap<MatrixReplyTarget>>,
}

impl MatrixTransport {
    pub(crate) fn new() -> Self {
        Self {
            secret_store: OnceLock::new(),
            clients: Arc::new(StdMutex::new(HashMap::new())),
            in_flight: Arc::new(CorrelationMap::new()),
        }
    }

    /// Accessors for [`super::outbound`], a sibling module that reads (but
    /// never owns) the client registry and correlation map.
    pub(super) fn in_flight(&self) -> &CorrelationMap<MatrixReplyTarget> {
        &self.in_flight
    }

    /// The binding's currently-connected client, if its sync task is up.
    /// `None` is routine (disconnected/backing off) — relay callers log and
    /// drop, never error.
    pub(super) fn client_for(&self, agent_id: &str, binding_id: &str) -> Option<Arc<MatrixClient>> {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(agent_id.to_string(), binding_id.to_string()))
            .cloned()
    }

    /// `pub(super)`: also used by `outbound`'s tests, which register a
    /// wiremock-backed client directly instead of running a sync task.
    /// `cfg(test)`: production registration happens inside
    /// [`run_sync_loop_inner`], which holds the registry rather than
    /// `&self`.
    #[cfg(test)]
    pub(super) fn register_client_for_tests(&self, agent_id: &str, binding_id: &str, client: Arc<MatrixClient>) {
        register_client(&self.clients, agent_id, binding_id, client);
    }

    /// Drops every correlation entry belonging to `(agent_id, binding_id)`
    /// — the `DELETE .../matrix/connection` route's eager half, so a
    /// completion still in flight when the connection is torn down finds
    /// nothing to relay to instead of going out through a client whose
    /// secrets were just deleted. `pub` for the ao-server route.
    pub fn invalidate_binding(&self, agent_id: &str, binding_id: &str) {
        for (thread_id, target) in self.in_flight.snapshot() {
            if target.agent_id == agent_id && target.binding_id == binding_id {
                self.in_flight.remove(&thread_id);
            }
        }
    }

    /// Leaves a room on behalf of the `DELETE .../matrix/rooms/{room_id}`
    /// route, through the binding's live client. Best-effort by contract:
    /// a disconnected binding (or a failed leave) is logged, not reported —
    /// the allow-list removal the route also performs is the actual
    /// authorization boundary. `pub` for the ao-server route.
    pub async fn leave_room_best_effort(&self, agent_id: &str, binding_id: &str, room_id: &str) {
        let Some(client) = self.client_for(agent_id, binding_id) else {
            debug!(agent_id = %agent_id, binding_id = %binding_id, room_id = %room_id, "MatrixTransport: not connected, skipping room leave (unlink still applies)");
            return;
        };
        match client.leave_room(room_id).await {
            Ok(()) => info!(agent_id = %agent_id, binding_id = %binding_id, room_id = %room_id, "MatrixTransport: left room after unlink"),
            Err(e) => warn!(agent_id = %agent_id, binding_id = %binding_id, room_id = %room_id, "MatrixTransport: failed to leave room after unlink (unlink still applies): {e}"),
        }
    }
    /// Returns the lazily-opened secret store, opening it on first use.
    /// `OnceLock::set` resolves a first-use race safely: at most one
    /// caller's store wins and everyone reads it back via `get()`.
    fn secret_store(&self) -> Result<&ChannelSecretStore, ao_engine_tools_provider_config::channel_secret_store::ChannelSecretStoreError> {
        if let Some(store) = self.secret_store.get() {
            return Ok(store);
        }
        let store = ChannelSecretStore::open()?;
        let _ = self.secret_store.set(store);
        Ok(self.secret_store.get().expect("secret store was just initialized above"))
    }

    /// Resolves one secret role for the binding, logging and returning
    /// `None` on any store failure or absence — callers treat "no secret" as
    /// "not runnable yet", not a hard failure. Never logs the secret itself.
    fn resolve_secret(&self, agent_id: &str, binding_id: &str, role: &str) -> Option<String> {
        match self.secret_store() {
            Ok(store) => match store.get(agent_id, binding_id, role) {
                Ok(secret) => secret,
                Err(e) => {
                    warn!(agent_id = %agent_id, binding_id = %binding_id, role = %role, "MatrixTransport: failed to read secret: {e}");
                    None
                }
            },
            Err(e) => {
                warn!(agent_id = %agent_id, binding_id = %binding_id, "MatrixTransport: failed to open secret store: {e}");
                None
            }
        }
    }

    /// Get-or-create for secrets the client generates itself (device id,
    /// store passphrase): read the vaulted value, and only mint + persist a
    /// fresh one when absent. A write failure is non-fatal to the caller —
    /// the minted value is still usable for this process's lifetime, at the
    /// cost of a fresh crypto identity on next restart (logged loudly).
    fn resolve_or_mint_secret(&self, agent_id: &str, binding_id: &str, role: &str) -> Option<String> {
        if let Some(existing) = self.resolve_secret(agent_id, binding_id, role) {
            return Some(existing);
        }
        let minted = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        match self.secret_store() {
            Ok(store) => {
                if let Err(e) = store.set(agent_id, binding_id, role, &minted) {
                    warn!(agent_id = %agent_id, binding_id = %binding_id, role = %role, "MatrixTransport: failed to vault generated secret (a fresh one will be minted on restart): {e}");
                }
            }
            Err(e) => {
                warn!(agent_id = %agent_id, binding_id = %binding_id, role = %role, "MatrixTransport: secret store unavailable, generated secret is process-local only: {e}");
            }
        }
        Some(minted)
    }
}

#[async_trait]
impl ChannelTransport for MatrixTransport {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Matrix
    }

    fn fingerprint(&self, agent: &AgentProfile, binding: &ChannelBinding) -> Option<String> {
        let ChannelKindConfig::Matrix { .. } = &binding.kind_config else {
            return None;
        };
        // Token is redacted from the fingerprint's tail by construction: it
        // lives in the secret store, never on `ChannelKindConfig`, so the
        // Debug output never includes it. A rotated token *or* a config
        // change restarts the sync task.
        let token = self.resolve_secret(&agent.id, &binding.binding_id, MATRIX_TOKEN_SECRET_ROLE)?;
        Some(format!("{token}|{:?}", binding.kind_config))
    }

    fn spawn(&self, ctx: ChannelRunContext, cancel: CancellationToken) -> JoinHandle<()> {
        let token = self.resolve_secret(&ctx.agent_id, &ctx.binding_id, MATRIX_TOKEN_SECRET_ROLE);
        let device_id =
            self.resolve_secret(&ctx.agent_id, &ctx.binding_id, MATRIX_DEVICE_ID_SECRET_ROLE);
        let store_passphrase = self.resolve_or_mint_secret(
            &ctx.agent_id,
            &ctx.binding_id,
            MATRIX_STORE_PASSPHRASE_SECRET_ROLE,
        );
        let clients = Arc::clone(&self.clients);
        let in_flight = Arc::clone(&self.in_flight);

        tokio::spawn(async move {
            let (Some(token), Some(store_passphrase)) = (token, store_passphrase) else {
                warn!(
                    agent_id = %ctx.agent_id,
                    binding_id = %ctx.binding_id,
                    "MatrixTransport: token or store passphrase unavailable at spawn time, not starting sync task"
                );
                return;
            };
            run_sync_loop(ctx, token, device_id, store_passphrase, clients, in_flight, cancel).await;
        })
    }

    /// Drops the thread's outbound-relay mapping. Called by
    /// `ChannelBridge::invalidate_thread` (which broadcasts to every
    /// transport) whenever a binding is torn down or a conversation is
    /// unlinked, so a completion still in flight has nothing left to relay
    /// to — the same contract `TelegramTransport::invalidate_thread`
    /// documents.
    fn invalidate_thread(&self, thread_id: &str) {
        self.in_flight.remove(thread_id);
    }

    /// The A4 outbound observer: relays finished replies back to their
    /// rooms and drives typing heartbeats (see [`super::outbound`]).
    fn spawn_outbound_observer(
        self: Arc<Self>,
        persistence: Arc<PersistenceLayer>,
        lease_gate: Arc<LeaseGate>,
        event_bus: Arc<EventBus>,
        shutdown_rx: watch::Receiver<()>,
    ) -> Option<JoinHandle<()>> {
        Some(tokio::spawn(async move {
            outbound::run_outbound_observer(self, persistence, lease_gate, event_bus, shutdown_rx).await;
        }))
    }
}

/// The per-binding sync loop. Runs until `cancel` fires, the binding
/// disappears/disables, or the token turns out dead. The wrapper owns the
/// client-registry lifecycle: whatever [`run_sync_loop_inner`] last
/// registered for this binding is removed when the task exits, so the
/// outbound relay never sends through a client whose task is gone.
#[allow(clippy::too_many_arguments)]
async fn run_sync_loop(
    ctx: ChannelRunContext,
    token: String,
    device_id: Option<String>,
    store_passphrase: String,
    clients: ClientRegistry,
    in_flight: Arc<CorrelationMap<MatrixReplyTarget>>,
    cancel: CancellationToken,
) {
    let agent_id = ctx.agent_id.clone();
    let binding_id = ctx.binding_id.clone();
    run_sync_loop_inner(ctx, token, device_id, store_passphrase, Arc::clone(&clients), in_flight, cancel).await;
    // Deregister on every exit path — cancellation included. A relay racing
    // this removal finds `client_for` → `None` and drops with a warning,
    // which is the designed posture.
    unregister_client(&clients, &agent_id, &binding_id);
}

/// Registers the binding's live client in the shared registry. Free
/// functions (not `MatrixTransport` methods) because the sync-loop
/// wrapper/inner hold the registry, not `&self`; `pub(super)` on
/// `register_client` so `outbound`'s tests can seed the same registry.
pub(super) fn register_client(
    clients: &ClientRegistry,
    agent_id: &str,
    binding_id: &str,
    client: Arc<MatrixClient>,
) {
    clients
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((agent_id.to_string(), binding_id.to_string()), client);
}

fn unregister_client(clients: &ClientRegistry, agent_id: &str, binding_id: &str) {
    clients
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(agent_id.to_string(), binding_id.to_string()));
}

/// The loop body, split from [`run_sync_loop`] so the wrapper can guarantee
/// client-registry cleanup on every exit path.
#[allow(clippy::too_many_arguments)]
async fn run_sync_loop_inner(
    ctx: ChannelRunContext,
    token: String,
    mut device_id: Option<String>,
    store_passphrase: String,
    clients: ClientRegistry,
    in_flight: Arc<CorrelationMap<MatrixReplyTarget>>,
    cancel: CancellationToken,
) {
    // Restore the durable cursor so a backend restart resumes from the last
    // handled batch instead of re-syncing (and re-delivering) history.
    // `seen_event_ids` rides along as the restart-surviving dedup list.
    let (mut since, mut seen_event_ids) =
        load_cursor(&ctx.persistence.channel_cursors, &ctx.agent_id, &ctx.binding_id).await;
    // Process-local `event_id → sender` map feeding the reply-to-bot
    // addressing check (see `inbound::RecentSenders`).
    let mut recent_senders = RecentSenders::new(SEEN_EVENT_IDS_CAP);

    // The initial sync on a fresh cursor exists only to establish
    // `next_batch`: delivering its (arbitrarily large) backlog as live
    // messages would spam every room the bot has ever been in. A3's handler
    // gets a flag for exactly this.
    let mut initial_sync_pending = since.is_none();

    let store_dir = ctx
        .persistence
        .data_root
        .root()
        .join("agents")
        .join(&ctx.agent_id)
        .join("matrix");

    // Resolve identity + device id lazily at first (re)connect: a binding
    // created through the A4 password-login route has both vaulted already;
    // a raw-token binding discovers them via `preflight_whoami` once and
    // vaults the device id from then on.
    let mut client: Option<Arc<MatrixClient>> = None;

    loop {
        // Re-read the profile every iteration (queue-manager pump pattern):
        // a mid-flight disable/remove stops the loop on its own cadence.
        let mut profile = match ctx.persistence.agents.get(&ctx.agent_id).await {
            Ok(Some(profile)) => profile,
            Ok(None) => {
                debug!(agent_id = %ctx.agent_id, "MatrixTransport: agent no longer exists, stopping sync task");
                return;
            }
            Err(e) => {
                warn!(agent_id = %ctx.agent_id, "MatrixTransport: failed to re-read agent profile: {e}");
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
                if wait_or_cancelled(&cancel, jittered_backoff()).await {
                    return;
                }
                continue;
            }
        };
        let Some(binding) = profile.channels.iter().find(|b| b.binding_id == ctx.binding_id) else {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: binding removed, stopping sync task");
            return;
        };
        if !binding.enabled {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: binding disabled, stopping sync task");
            return;
        }
        let ChannelKindConfig::Matrix { homeserver_url, bot_user_id, .. } = &binding.kind_config else {
            warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: binding kind_config is not Matrix, stopping sync task");
            return;
        };
        let homeserver_url = homeserver_url.clone();
        let bot_user_id = bot_user_id.clone();
        // Cloned so the batch handler can take `&mut profile` (pairing
        // clears the pending code in memory, mirroring `try_link_chat`)
        // while still reading the gate config.
        let kind_config = binding.kind_config.clone();
        let inline_allowed_senders = binding.allowed_senders.clone();

        // (Re)connect phase: build the client if we don't have one. A dead
        // token is terminal; anything else backs off and retries.
        if client.is_none() {
            let identity = match &bot_user_id {
                Some(user_id) => match &device_id {
                    Some(device_id) => Some((user_id.clone(), device_id.clone())),
                    None => None,
                },
                None => None,
            };
            let (user_id, resolved_device_id) = match identity {
                Some(pair) => pair,
                None => {
                    // Raw-token binding: discover identity, vault the
                    // device id so the next restart skips this call.
                    match preflight_whoami(&homeserver_url, &token).await {
                        Ok(identity) => {
                            let Some(found_device_id) = identity.device_id else {
                                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: token has no device id (whoami returned none); cannot restore an E2EE session, stopping sync task");
                                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Disconnected);
                                return;
                            };
                            if let Err(e) = ChannelSecretStore::open().and_then(|store| {
                                store.set(
                                    &ctx.agent_id,
                                    &ctx.binding_id,
                                    MATRIX_DEVICE_ID_SECRET_ROLE,
                                    &found_device_id,
                                )
                            }) {
                                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: failed to vault discovered device id (will re-discover on restart): {e}");
                            }
                            device_id = Some(found_device_id.clone());
                            (identity.user_id, found_device_id)
                        }
                        Err(e) => {
                            if terminal_or_backoff(&ctx, &e, &cancel).await {
                                return;
                            }
                            continue;
                        }
                    }
                }
            };
            let params = MatrixConnectionParams {
                homeserver_url: homeserver_url.clone(),
                access_token: token.clone(),
                bot_user_id: user_id,
                device_id: resolved_device_id,
                store_dir: store_dir.clone(),
                store_passphrase: store_passphrase.clone(),
                // One "who owns this binding" answer at both layers (D4):
                // the lease store's owner_id names the SDK's
                // cross-process store locks too.
                lock_holder_name: ctx.owner_id.clone(),
            };
            match MatrixClient::connect(&params).await {
                Ok(connected) => {
                    let connected = Arc::new(connected);
                    // Register before the first sync so a reply to a
                    // message delivered by that very sync can relay through
                    // this client. Reconnects overwrite the stale entry.
                    register_client(&clients, &ctx.agent_id, &ctx.binding_id, Arc::clone(&connected));
                    client = Some(connected);
                }
                Err(e) => {
                    if terminal_or_backoff(&ctx, &e, &cancel).await {
                        return;
                    }
                    continue;
                }
            }
        }
        let live = Arc::clone(client.as_ref().expect("client was just connected above"));

        // Sync phase: one long-poll. `sync_once`'s own 30s timeout is the
        // poll cadence; `cancel` interrupts it immediately.
        let result = tokio::select! {
            _ = cancel.cancelled() => return,
            result = live.sync_once(since.clone()) => result,
        };
        match result {
            Ok(batch) => {
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Connected);
                if let Err(e) = handle_sync_batch(
                    &ctx,
                    &mut profile,
                    &batch,
                    &live,
                    &kind_config,
                    &inline_allowed_senders,
                    initial_sync_pending,
                    &mut seen_event_ids,
                    &mut recent_senders,
                    &in_flight,
                )
                .await
                {
                    // The cursor is deliberately NOT persisted here: the next
                    // sync re-delivers this batch and `seen_event_ids`
                    // absorbs whatever already dispatched. A handler error
                    // means a persistence failure, so advancing the cursor
                    // would lose messages permanently.
                    warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: batch handling failed, will re-deliver: {e}");
                    continue;
                }
                if initial_sync_pending {
                    debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: initial sync complete, backlog recorded but not delivered");
                    initial_sync_pending = false;
                }
                // Persist only after the batch is fully handled so the
                // crash re-delivery window is one batch, never more.
                since = Some(batch.next_batch);
                if let Err(e) = ctx
                    .persistence
                    .channel_cursors
                    .set(&ctx.agent_id, &ctx.binding_id, &ChannelCursor::Matrix { next_batch: since.clone(), seen_event_ids: seen_event_ids.clone() })
                    .await
                {
                    warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: failed to persist cursor: {e}");
                }
            }
            Err(e) => {
                if e.kind == MatrixClientErrorKind::UnknownToken {
                    warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: token rejected during sync, stopping until reconfigured: {e}");
                    ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Disconnected);
                    return;
                }
                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, kind = ?e.kind, "MatrixTransport: sync failed: {e}");
                ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
                if wait_or_cancelled(&cancel, jittered_backoff()).await {
                    return;
                }
                // A store-level failure can heal by rebuilding the client
                // (e.g. the other process released the cross-process lock);
                // network failures reuse the existing client.
                if e.kind == MatrixClientErrorKind::Store {
                    client = None;
                }
            }
        }
    }
}

/// Handles one successful sync batch (A3): invites first, then per event the
/// gate chain from `inbound` — dedup → self/notice/edit guards → pairing →
/// linked-room allow-list → addressing → per-room thread resolution →
/// `submit_inbound_message`. An initial-sync batch only records its events
/// (dedup + reply-to-bot map) and submits nothing: the backlog guard.
///
/// Per-event failures log and move on (one bad event must not poison its
/// batch); only a whole-batch failure returns `Err`, which tells the caller
/// to skip cursor persistence so the batch re-delivers once.
#[allow(clippy::too_many_arguments)]
async fn handle_sync_batch(
    ctx: &ChannelRunContext,
    profile: &mut AgentProfile,
    batch: &SyncBatch,
    client: &MatrixClient,
    kind_config: &ChannelKindConfig,
    inline_allowed_senders: &[String],
    initial_sync_pending: bool,
    seen_event_ids: &mut Vec<String>,
    recent_senders: &mut RecentSenders,
    in_flight: &CorrelationMap<MatrixReplyTarget>,
) -> Result<(), ao_protocol::error::AoError> {
    let ChannelKindConfig::Matrix {
        auto_accept_invites,
        require_addressing_in_rooms,
        command_prefix,
        ..
    } = kind_config
    else {
        return Ok(());
    };
    let bot_user_id = client.bot_user_id().to_string();

    // The allow-list is read once per batch (not per event) — a
    // mid-batch unlink takes effect on the next batch, same staleness
    // window as Telegram's per-update read.
    let has_work = !batch.invites.is_empty() || (!initial_sync_pending && !batch.joined.is_empty());
    let allowed_senders = if has_work {
        match ctx
            .persistence
            .linked_senders
            .get_or_backfill(&ctx.agent_id, &ctx.binding_id, inline_allowed_senders)
            .await
        {
            Ok(senders) => senders,
            Err(e) => {
                // Mirroring Telegram's drop-on-store-error posture: a
                // transient read failure drops the batch (the homeserver
                // holds it until the cursor advances) rather than spinning
                // a retry loop against a broken store.
                warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: failed to read linked senders, dropping batch: {e}");
                return Ok(());
            }
        }
    } else {
        Vec::new()
    };

    // Bootstrap window (plan §4.3 amendment): an invite from an UNLINKED
    // inviter is also accepted while this binding holds a live pairing
    // code — without it, first contact deadlocks (pairing requires the bot
    // in the room; every first invite is from an unlinked user). Presence
    // isn't authorization: the room still can't reach the agent until a
    // `pair` command lands, the allow-list gate below is untouched.
    let pending_code_live = profile
        .channels
        .iter()
        .find(|b| b.binding_id == ctx.binding_id)
        .and_then(|b| b.pending_pairing_code.as_ref())
        .is_some_and(|code| code.expires_at_unix > Utc::now().timestamp());

    for invite in &batch.invites {
        let inviter_linked = allowed_senders.iter().any(|sender| sender == &invite.inviter);
        if !should_accept_invite(*auto_accept_invites, inviter_linked, pending_code_live) {
            debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, room_id = %invite.room_id, inviter = %invite.inviter, "MatrixTransport: ignoring room invite (auto-accept off, or inviter not linked and no live pairing code)");
            continue;
        }
        match client.join_room(&invite.room_id).await {
            Ok(()) => info!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, room_id = %invite.room_id, "MatrixTransport: accepted room invite (linked inviter or live pairing code)"),
            Err(e) => warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, room_id = %invite.room_id, "MatrixTransport: failed to join invited room: {e}"),
        }
    }

    if initial_sync_pending {
        // Backlog guard (plan §4.4): the initial sync's timeline events are
        // history, not live traffic. They still feed the dedup list and the
        // reply-to-bot map — a reply to an old bot message right after
        // startup should count as addressed — but nothing is dispatched and
        // no thread is minted.
        for room in &batch.joined {
            for event in &room.events {
                push_seen(seen_event_ids, &event.event_id);
                recent_senders.record(&event.event_id, &event.sender);
            }
        }
        return Ok(());
    }

    for room in &batch.joined {
        for event in &room.events {
            if let Some(reason) = pre_gate(event, &bot_user_id, seen_event_ids) {
                debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, event_id = %event.event_id, ?reason, "MatrixTransport: dropping event");
                continue;
            }
            push_seen(seen_event_ids, &event.event_id);
            recent_senders.record(&event.event_id, &event.sender);

            // Pairing runs before the allow-list: an unlinked room is
            // exactly where a pairing command must work. The reply is an
            // `m.notice` so other bots never treat it as conversation.
            if let Some(code) = parse_pair_command(&event.body, command_prefix) {
                let linked = match try_link_room(
                    &ctx.persistence,
                    profile,
                    &ctx.binding_id,
                    &room.room_id,
                    &event.sender,
                    code,
                    Utc::now().timestamp(),
                )
                .await
                {
                    Ok(linked) => linked,
                    Err(e) => {
                        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: failed to persist pairing link: {e}");
                        false
                    }
                };
                if linked {
                    info!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, room_id = %room.room_id, sender = %event.sender, "MatrixTransport: pairing succeeded, room and sender linked");
                } else {
                    info!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, room_id = %room.room_id, sender = %event.sender, "MatrixTransport: pairing attempt rejected (code missing, expired, or mismatched)");
                }
                let reply = if linked { PAIRING_SUCCESS_REPLY } else { PAIRING_FAILURE_REPLY };
                if let Err(e) = client.send_notice(&room.room_id, reply).await {
                    warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, room_id = %room.room_id, "MatrixTransport: failed to send pairing reply: {e}");
                }
                continue;
            }

            if !allowed_senders.iter().any(|sender| sender == &room.room_id) {
                debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, room_id = %room.room_id, "MatrixTransport: dropping message from unlinked room");
                continue;
            }

            let reply_to_sender = event
                .reply_to_event_id
                .as_deref()
                .and_then(|event_id| recent_senders.sender_of(event_id));
            let Some(text) = apply_addressing(
                &event.body,
                room.is_direct,
                &event.mentioned_user_ids,
                reply_to_sender,
                &bot_user_id,
                *require_addressing_in_rooms,
                command_prefix,
            ) else {
                debug!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, room_id = %room.room_id, "MatrixTransport: room message does not address the bot, skipping");
                continue;
            };

            // Resolved only now, after every gate has passed — a dropped
            // message never mints a per-conversation thread it will never
            // use (same placement as Telegram's `handle_update`).
            let Some(thread_id) =
                resolve_matrix_conversation_thread(ctx, &room.room_id, Utc::now()).await
            else {
                warn!(agent_id = %ctx.agent_id, room_id = %room.room_id, "MatrixTransport: failed to resolve a per-conversation bridge thread, dropping message");
                continue;
            };

            // Record the reply target right before dispatch (mirrors
            // Telegram's `in_flight.record`): `AgentEvent`s carry no room
            // id, so the outbound observer resolves thread→room from this
            // map at `RunStarted`/`RunEnded`. `trigger_event_id` is carried
            // for B8's ack reactions.
            in_flight.record(
                &thread_id,
                MatrixReplyTarget {
                    agent_id: ctx.agent_id.clone(),
                    binding_id: ctx.binding_id.clone(),
                    room_id: room.room_id.clone(),
                    trigger_event_id: event.event_id.clone(),
                },
            );

            // The room id is the Matrix conversation boundary; the sender
            // MXID doubles as display name until a profile lookup lands.
            if let Err(e) = submit_inbound_message(
                ctx,
                profile,
                &thread_id,
                ChannelKind::Matrix,
                &room.room_id,
                &event.sender,
                None,
                &text,
                Some(text.clone()),
            )
            .await
            {
                warn!(agent_id = %ctx.agent_id, "MatrixTransport: failed to deliver inbound message: {e}");
            }
        }
    }
    Ok(())
}

/// Appends to the dedup list, evicting oldest-first past the cap. The list
/// is persisted with the cursor, so it must stay YAML-sized.
fn push_seen(seen_event_ids: &mut Vec<String>, event_id: &str) {
    seen_event_ids.push(event_id.to_string());
    if seen_event_ids.len() > SEEN_EVENT_IDS_CAP {
        let overflow = seen_event_ids.len() - SEEN_EVENT_IDS_CAP;
        seen_event_ids.drain(..overflow);
    }
}

/// Restores the persisted sync cursor: `next_batch` becomes the first
/// `since=`, `seen_event_ids` is A3's dedup list. A missing, unreadable, or
/// wrong-kind cursor (e.g. a binding that was previously Telegram under the
/// same id) starts fresh rather than failing the loop.
async fn load_cursor(
    cursors: &ao_persistence::channel_cursor_store::ChannelCursorStore,
    agent_id: &str,
    binding_id: &str,
) -> (Option<String>, Vec<String>) {
    match cursors.get(agent_id, binding_id).await {
        Ok(Some(ChannelCursor::Matrix { next_batch, seen_event_ids })) => {
            (next_batch, seen_event_ids)
        }
        Ok(Some(other)) => {
            warn!(agent_id = %agent_id, binding_id = %binding_id, ?other, "MatrixTransport: persisted cursor is not a Matrix cursor, starting fresh");
            (None, Vec::new())
        }
        Ok(None) => (None, Vec::new()),
        Err(e) => {
            warn!(agent_id = %agent_id, binding_id = %binding_id, "MatrixTransport: failed to load persisted cursor, starting fresh: {e}");
            (None, Vec::new())
        }
    }
}

/// Terminal-vs-backoff decision shared by the preflight and connect paths:
/// returns `true` when the loop must stop (dead token, or cancelled while
/// backing off).
async fn terminal_or_backoff(
    ctx: &ChannelRunContext,
    error: &MatrixClientError,
    cancel: &CancellationToken,
) -> bool {
    if error.kind == MatrixClientErrorKind::UnknownToken {
        warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, "MatrixTransport: access token rejected, stopping until reconfigured: {error}");
        ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Disconnected);
        return true;
    }
    warn!(agent_id = %ctx.agent_id, binding_id = %ctx.binding_id, kind = ?error.kind, "MatrixTransport: connect failed, backing off: {error}");
    ctx.connection_state.set(&ctx.agent_id, &ctx.binding_id, ChannelConnectionState::Reconnecting);
    wait_or_cancelled(cancel, jittered_backoff()).await
}

/// `ERROR_BACKOFF` plus up to 50% random jitter, so a fleet of bindings
/// knocked down by the same homeserver outage doesn't retry in lockstep.
fn jittered_backoff() -> Duration {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let jitter_millis = u64::from(bytes[0]) * 1000 / 255 * ERROR_BACKOFF.as_millis() as u64 / 2 / 1000;
    ERROR_BACKOFF + Duration::from_millis(jitter_millis)
}

/// Sleeps for `dur` unless `cancel` fires first. Returns `true` if cancelled.
async fn wait_or_cancelled(cancel: &CancellationToken, dur: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(dur) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_persistence::channel_cursor_store::ChannelCursorStore;
    use ao_persistence::paths::DataRoot;

    #[test]
    fn jittered_backoff_stays_within_bounds() {
        for _ in 0..64 {
            let d = jittered_backoff();
            assert!(d >= ERROR_BACKOFF);
            assert!(d <= ERROR_BACKOFF + ERROR_BACKOFF / 2 + Duration::from_millis(1));
        }
    }

    fn cursor_store() -> (ChannelCursorStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        (ChannelCursorStore::new(DataRoot::new(tmp.path().to_path_buf())), tmp)
    }

    #[tokio::test]
    async fn cursor_restore_starts_fresh_when_nothing_persisted() {
        let (cursors, _tmp) = cursor_store();
        let (since, seen) = load_cursor(&cursors, "agent", "binding").await;
        assert_eq!(since, None);
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn cursor_restore_round_trips_matrix_cursor() {
        let (cursors, _tmp) = cursor_store();
        cursors
            .set(
                "agent",
                "binding",
                &ChannelCursor::Matrix {
                    next_batch: Some("s725".to_string()),
                    seen_event_ids: vec!["$evt1".to_string()],
                },
            )
            .await
            .expect("persist");
        let (since, seen) = load_cursor(&cursors, "agent", "binding").await;
        assert_eq!(since.as_deref(), Some("s725"));
        assert_eq!(seen, vec!["$evt1".to_string()]);
    }

    #[tokio::test]
    async fn cursor_restore_discards_wrong_kind() {
        let (cursors, _tmp) = cursor_store();
        cursors
            .set("agent", "binding", &ChannelCursor::Telegram { offset: Some(42) })
            .await
            .expect("persist");
        let (since, seen) = load_cursor(&cursors, "agent", "binding").await;
        assert_eq!(since, None);
        assert!(seen.is_empty());
    }
}
