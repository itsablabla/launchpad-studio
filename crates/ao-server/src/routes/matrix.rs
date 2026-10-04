//! Per-agent Matrix connection endpoints (guide/MATRIX_PLAN.md A4).
//!
//! Mirrors `routes/telegram.rs`'s contract with Matrix's differences folded
//! in: the secret is an access token (+ device id) vaulted under the
//! `MATRIX_*_SECRET_ROLE` roles instead of a single bot token; connection
//! accepts **either** a raw token **or** a username/password pair (the
//! password path logs in via the SDK, keeps token + device id, discards the
//! password); and status additionally reports the live connection state.
//! The token never round-trips: write-only here, validated against the
//! homeserver (`whoami` / `login`) before anything is stored, and never
//! echoed back over HTTP.
//!
//! Matrix needs no `provision_bridge_thread` call on connect: unlike
//! Telegram's single dedicated bridge thread, Matrix mints per-room
//! conversation threads on demand in the inbound pipeline (and
//! `ChannelBridge::reconcile` skips `bridge_thread_id` for Matrix for the
//! same reason).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use ao_engine::channels::matrix::{
    password_login, preflight_whoami, MatrixClientError, MatrixClientErrorKind,
};
use ao_engine::AppState;
use ao_engine_tools_provider_config::channel_secret_store::{
    ChannelSecretStore, ChannelSecretStoreError,
};
use ao_engine_tools_provider_config::{
    MATRIX_DEVICE_ID_SECRET_ROLE, MATRIX_STORE_PASSPHRASE_SECRET_ROLE, MATRIX_TOKEN_SECRET_ROLE,
};
use ao_protocol::agent::{
    AgentProfile, ChannelBinding, ChannelKind, ChannelKindConfig, MatrixEngagementConfig,
    MatrixStreamEdits, MatrixThreadMode, PairingCode,
};
use ao_protocol::channel_connection_state::ChannelConnectionState;
use ao_protocol::channel_cursor::ChannelCursor;
use ao_protocol::error::AoError;

use crate::error::AppError;

/// v1 supports one Matrix binding per agent, fixed id — mirrors Telegram's
/// singleton `"telegram"` binding and keeps the routes/frontend simple.
/// (`ChannelKindConfig::Matrix` is per-binding homeserver-scoped, so
/// multi-homeserver support is a matter of parameterizing this later, not
/// a schema change.)
const MATRIX_BINDING_ID: &str = "matrix";

fn map_store_err(e: ChannelSecretStoreError) -> AppError {
    AppError(AoError::Internal(format!("matrix secret store: {e}")))
}

fn open_secret_store() -> Result<ChannelSecretStore, AppError> {
    ChannelSecretStore::open().map_err(map_store_err)
}

/// Maps a connection-attempt failure onto a setup-appropriate validation
/// error. Deliberately generic messages: the raw SDK error chain (homeserver
/// URLs are fine, but error bodies can echo request detail) stays in the
/// logs, not the HTTP response.
fn map_connect_err(e: &MatrixClientError) -> AppError {
    let message = match e.kind {
        MatrixClientErrorKind::UnknownToken => {
            "the homeserver rejected that access token".to_string()
        }
        MatrixClientErrorKind::Forbidden => "invalid username or password".to_string(),
        MatrixClientErrorKind::Unreachable => {
            "the homeserver could not be reached — check the URL".to_string()
        }
        MatrixClientErrorKind::Store | MatrixClientErrorKind::Other => {
            "the homeserver rejected the connection attempt".to_string()
        }
    };
    AppError(AoError::ValidationError(message))
}

/// Returns a mutable reference to `profile`'s Matrix binding, inserting a
/// disabled one (with the given homeserver URL and defaults for every gate)
/// first if it doesn't have one yet.
fn matrix_binding_mut_or_default<'a>(
    profile: &'a mut AgentProfile,
    homeserver_url: &str,
) -> &'a mut ChannelBinding {
    if profile.channel_of_kind(ChannelKind::Matrix).is_none() {
        profile.channels.push(ChannelBinding {
            binding_id: MATRIX_BINDING_ID.to_string(),
            kind: ChannelKind::Matrix,
            enabled: false,
            bridge_thread_id: None,
            allowed_senders: Vec::new(),
            pending_pairing_code: None,
            kind_config: ChannelKindConfig::Matrix {
                bot_user_id: None,
                homeserver_url: homeserver_url.to_string(),
                auto_accept_invites: true,
                require_addressing_in_rooms: true,
                command_prefix: "!agent".to_string(),
                thread_mode: MatrixThreadMode::default(),
                stream_edits: MatrixStreamEdits::default(),
                process_edits: false,
                ack_reactions: true,
                feedback_reactions: true,
                send_read_receipts: true,
                download_attachments: true,
                download_max_bytes: 25 * 1024 * 1024,
                bot_display_name: None,
                engagement: MatrixEngagementConfig::default(),
            },
        });
    }
    profile.channel_of_kind_mut(ChannelKind::Matrix).expect("just inserted above if missing")
}

#[derive(Debug, Deserialize)]
pub struct SetMatrixConnectionRequest {
    pub homeserver_url: String,
    /// Raw access token path (advanced setup).
    pub access_token: Option<String>,
    /// Password-login path (friendly setup): both or neither.
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SetMatrixConnectionResponse {
    pub user_id: String,
}

/// `PUT /agents/{agent_id}/matrix/connection` — validate and store a
/// connection: either `{ homeserver_url, access_token }` or
/// `{ homeserver_url, username, password }`.
///
/// The token path validates via `whoami` (and requires the token to carry a
/// device id — a device-less token can never restore an E2EE session, so
/// it's rejected here rather than failing the sync loop at runtime). The
/// password path logs in via the SDK and keeps only the returned token +
/// device id; the password is never stored. On success the resolved bot
/// MXID is cached onto the binding's `kind_config` so the frontend can
/// render it without ever holding a secret, and the binding is enabled.
pub async fn set_matrix_connection(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetMatrixConnectionRequest>,
) -> Result<Json<SetMatrixConnectionResponse>, AppError> {
    let homeserver_url = body.homeserver_url.trim().trim_end_matches('/').to_string();
    if homeserver_url.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "homeserver_url must not be empty".to_string(),
        )));
    }

    let access_token = body.access_token.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let username = body.username.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let password = body.password.as_deref().filter(|s| !s.is_empty());

    // Exactly one of the two auth shapes.
    let (user_id, device_id, token) = match (access_token, username, password) {
        (Some(token), None, None) => {
            let identity = preflight_whoami(&homeserver_url, token)
                .await
                .map_err(|e| map_connect_err(&e))?;
            let Some(device_id) = identity.device_id else {
                return Err(AppError(AoError::ValidationError(
                    "this token is not tied to a device (whoami returned no device_id); \
                     a device-bound session is required for end-to-end encryption — \
                     connect with the bot account's password instead"
                        .to_string(),
                )));
            };
            (identity.user_id, device_id, token.to_string())
        }
        (None, Some(username), Some(password)) => {
            let result = password_login(&homeserver_url, username, password)
                .await
                .map_err(|e| map_connect_err(&e))?;
            (result.user_id, result.device_id, result.access_token)
        }
        _ => {
            return Err(AppError(AoError::ValidationError(
                "provide either access_token or both username and password".to_string(),
            )));
        }
    };

    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Vault the session before touching the profile: if the profile write
    // below fails, an orphaned vaulted secret is harmless (overwritten on
    // the next connect, never read while the binding is disabled), whereas
    // the reverse order could enable a binding whose secrets never landed.
    let store = open_secret_store()?;
    store
        .set(&agent_id, MATRIX_BINDING_ID, MATRIX_TOKEN_SECRET_ROLE, &token)
        .map_err(map_store_err)?;
    store
        .set(&agent_id, MATRIX_BINDING_ID, MATRIX_DEVICE_ID_SECRET_ROLE, &device_id)
        .map_err(map_store_err)?;

    let binding = matrix_binding_mut_or_default(&mut profile, &homeserver_url);
    if let ChannelKindConfig::Matrix { homeserver_url: url, bot_user_id, .. } =
        &mut binding.kind_config
    {
        *url = homeserver_url;
        *bot_user_id = Some(user_id.clone());
    }
    binding.enabled = true;
    // A (re)connection invalidates any pending pairing code minted under
    // the previous account, and the previous account's sync cursor — a
    // `next_batch` token is only meaningful to the homeserver/account that
    // issued it.
    binding.pending_pairing_code = None;
    state
        .persistence
        .channel_cursors
        .set(
            &agent_id,
            MATRIX_BINDING_ID,
            &ChannelCursor::Matrix { next_batch: None, seen_event_ids: vec![] },
        )
        .await?;

    state.persistence.agents.update(&profile).await?;

    Ok(Json(SetMatrixConnectionResponse { user_id }))
}

/// `DELETE /agents/{agent_id}/matrix/connection` — disconnect: clear the
/// vaulted secrets and fully reset the binding. Removing the token
/// invalidates every room/user linkage and any pending pairing code, since
/// both were only meaningful while a specific account owned this agent's
/// Matrix presence.
pub async fn delete_matrix_connection(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let store = open_secret_store()?;
    for role in
        [MATRIX_TOKEN_SECRET_ROLE, MATRIX_DEVICE_ID_SECRET_ROLE, MATRIX_STORE_PASSPHRASE_SECRET_ROLE]
    {
        store.delete(&agent_id, MATRIX_BINDING_ID, role).map_err(map_store_err)?;
    }

    if let Some(mut profile) = state.persistence.agents.get(&agent_id).await? {
        let had_binding = profile.channel_of_kind(ChannelKind::Matrix).is_some();
        if let Some(binding) = profile.channel_of_kind_mut(ChannelKind::Matrix) {
            binding.enabled = false;
            binding.allowed_senders.clear();
            binding.pending_pairing_code = None;
            if let ChannelKindConfig::Matrix { bot_user_id, .. } = &mut binding.kind_config {
                *bot_user_id = None;
            }
        }
        if had_binding {
            state.persistence.agents.update(&profile).await?;
        }
        // Every sender authorized under the deleted connection loses that
        // authorization (mirrors `delete_telegram_token`'s store clear).
        state.persistence.linked_senders.clear(&agent_id, MATRIX_BINDING_ID).await?;
        // Eager, not reconcile-delayed: a completion still in flight must
        // find nothing to relay to now that the connection's secrets are
        // gone.
        state.matrix_transport.invalidate_binding(&agent_id, MATRIX_BINDING_ID);
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct MatrixStatusResponse {
    pub has_token: bool,
    pub homeserver_url: Option<String>,
    pub bot_user_id: Option<String>,
    pub enabled: bool,
    pub connection_state: ChannelConnectionState,
    pub linked: bool,
    /// Linked conversation ids: room ids (`!…`) only — the pairing flow
    /// also stores sender MXIDs (for the invite gate), which are not
    /// conversations and don't belong in this list.
    pub linked_rooms: Vec<String>,
    pub pending_pairing_code: Option<PairingCode>,
}

/// `GET /agents/{agent_id}/matrix/status` — non-secret status for the setup
/// modal. Never returns the token (or any secret). An expired pending
/// pairing code is reported as `null`, since callers only care whether it's
/// currently usable.
pub async fn get_matrix_status(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<MatrixStatusResponse>, AppError> {
    let profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let store = open_secret_store()?;
    let has_token = store
        .get(&agent_id, MATRIX_BINDING_ID, MATRIX_TOKEN_SECRET_ROLE)
        .map_err(map_store_err)?
        .is_some();

    let now_unix = chrono::Utc::now().timestamp();
    let binding = profile.channel_of_kind(ChannelKind::Matrix);
    let (homeserver_url, bot_user_id) = match binding.map(|b| &b.kind_config) {
        Some(ChannelKindConfig::Matrix { homeserver_url, bot_user_id, .. }) => {
            (Some(homeserver_url.clone()), bot_user_id.clone())
        }
        _ => (None, None),
    };
    let linked_rooms = state
        .persistence
        .linked_senders
        .get(&agent_id, MATRIX_BINDING_ID)
        .await?
        .map(|list| {
            list.senders.into_iter().filter(|sender| sender.starts_with('!')).collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(Json(MatrixStatusResponse {
        has_token,
        homeserver_url,
        bot_user_id,
        enabled: binding.map(|b| b.enabled).unwrap_or(false),
        connection_state: state.telegram_bridge.connection_state(&agent_id, MATRIX_BINDING_ID),
        linked: !linked_rooms.is_empty(),
        linked_rooms,
        pending_pairing_code: binding.and_then(|b| {
            b.pending_pairing_code.clone().filter(|code| !code.is_expired(now_unix))
        }),
    }))
}

#[derive(Debug, Serialize)]
pub struct CreatePairingCodeResponse {
    pub code: String,
    pub expires_at_unix: i64,
}

/// `POST /agents/{agent_id}/matrix/pairing-code` — mint a fresh pairing
/// code for the user to send the bot from the room they want to link
/// (`<prefix> pair <code>`). Regenerating overwrites any prior pending
/// code, so only the most recently issued code is ever valid. Requires the
/// binding to exist (a pairing code without a connected bot is useless).
pub async fn create_matrix_pairing_code(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<CreatePairingCodeResponse>, AppError> {
    let mut profile = state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    if profile.channel_of_kind(ChannelKind::Matrix).is_none() {
        return Err(AppError(AoError::ValidationError(
            "matrix is not configured for this agent — connect it first".to_string(),
        )));
    }

    let now_unix = chrono::Utc::now().timestamp();
    let pairing_code = PairingCode::generate(now_unix);
    profile
        .channel_of_kind_mut(ChannelKind::Matrix)
        .expect("checked above")
        .pending_pairing_code = Some(pairing_code.clone());
    state.persistence.agents.update(&profile).await?;

    Ok(Json(CreatePairingCodeResponse {
        code: pairing_code.code,
        expires_at_unix: pairing_code.expires_at_unix,
    }))
}

#[derive(Debug, Serialize)]
pub struct UnlinkRoomResponse {
    pub linked_rooms: Vec<String>,
}

/// `DELETE /agents/{agent_id}/matrix/rooms/{room_id}` — revoke a linked
/// room's access to this agent and leave it. Idempotent: unlinking a room
/// that isn't linked just returns the (unchanged) list.
///
/// Authorization lives in the `LinkedSenderStore` (written by the pairing
/// flow), so unlinking removes the room there — mirroring
/// `delete_telegram_chat`'s revocation, including the direct
/// outbound-relay invalidation so an in-flight reply doesn't land in a room
/// the user just unlinked. The room leave is best-effort hygiene on top of
/// the (already-enforced) authorization removal.
pub async fn delete_matrix_room(
    State(state): State<Arc<AppState>>,
    Path((agent_id, room_id)): Path<(String, String)>,
) -> Result<Json<UnlinkRoomResponse>, AppError> {
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    state
        .persistence
        .linked_senders
        .remove_sender(&agent_id, MATRIX_BINDING_ID, &room_id)
        .await?;

    // Drop the outbound relay's mapping for this room's conversation thread
    // (if one was ever minted), rather than waiting for binding teardown.
    let key = ao_protocol::conversation_registry::ConversationKey::new(room_id.clone());
    if let Some(row) = state
        .persistence
        .conversation_registry
        .get(&agent_id, MATRIX_BINDING_ID, &key)
        .await?
    {
        state.telegram_bridge.invalidate_thread(&row.thread_id);
    }

    state.matrix_transport.leave_room_best_effort(&agent_id, MATRIX_BINDING_ID, &room_id).await;

    let linked_rooms = state
        .persistence
        .linked_senders
        .get(&agent_id, MATRIX_BINDING_ID)
        .await?
        .map(|list| {
            list.senders.into_iter().filter(|sender| sender.starts_with('!')).collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(Json(UnlinkRoomResponse { linked_rooms }))
}
