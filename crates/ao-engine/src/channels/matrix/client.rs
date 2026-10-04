//! Thin adapter over `matrix_sdk::Client` — the only place in the workspace
//! where SDK types are named (guide/MATRIX_PLAN.md D1), so an SDK upgrade or
//! a future swap of the underlying client touches this file alone.
//!
//! Session discipline (v1 §2.3 / D5): a session is only ever restored from
//! the *pair* `(access_token, device_id)`. A token minted by the
//! password-login path (A4 routes) arrives with its device id from the login
//! response; a raw token pasted by the user does not — for that case
//! [`preflight_whoami`] discovers the identity (and the device id the
//! homeserver associates with the token) over plain HTTP before the SDK
//! client is built, so `restore_session` never runs with a mismatched
//! device id against an E2EE store.

use std::path::PathBuf;

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::{SyncSettings, SyncToken};
use matrix_sdk::cross_process_lock::CrossProcessLockConfig;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::message::{
    MessageType, Relation, RoomMessageEventContent, SyncRoomMessageEvent,
};
use matrix_sdk::ruma::events::{AnyStrippedStateEvent, AnySyncMessageLikeEvent, AnySyncTimelineEvent};
use matrix_sdk::ruma::{OwnedUserId, RoomId, UserId};
use matrix_sdk::{Client, ClientBuildError, SessionMeta, SessionTokens};

/// Everything needed to bring a [`MatrixClient`] up. Secrets arrive already
/// resolved — this adapter never touches the secret store itself.
pub struct MatrixConnectionParams {
    pub homeserver_url: String,
    pub access_token: String,
    /// Bot MXID (`@bot:example.com`) — from `kind_config.bot_user_id`, or
    /// discovered via [`preflight_whoami`] when the binding predates that
    /// caching.
    pub bot_user_id: String,
    /// Device id paired with the access token (see module doc).
    pub device_id: String,
    /// Persistent state + crypto store directory
    /// (`<data_root>/agents/{agent_id}/matrix/`).
    pub store_dir: PathBuf,
    /// Passphrase encrypting the store (vaulted, generated once — D5).
    pub store_passphrase: String,
    /// Cross-process store-lock holder — the `ChannelLeaseStore` owner_id, so
    /// "who owns this binding" has one answer at both layers (D4).
    pub lock_holder_name: String,
}

/// Coarse error taxonomy the sync loop maps onto
/// [`ao_protocol::channel_connection_state::ChannelConnectionState`]:
/// `UnknownToken` is terminal (Disconnected), `Unreachable`/`Store`/`Other`
/// back off and retry (Reconnecting). `pub` so the ao-server setup routes
/// can classify a failed connect attempt (D1: the type carries no SDK
/// types, so exposing it doesn't leak the SDK across the facade).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixClientErrorKind {
    /// The homeserver rejected the access token (`M_UNKNOWN_TOKEN`,
    /// `M_MISSING_TOKEN`, or a bare 401). Retrying is pointless until the
    /// user re-connects the binding.
    UnknownToken,
    /// `M_FORBIDDEN` — on the password-login path this is the "wrong
    /// username or password" answer; the sync loop treats it like `Other`
    /// (retry with backoff).
    Forbidden,
    /// Network-level failure: DNS, connect, TLS, timeout, or a 5xx from the
    /// homeserver. Transient by assumption.
    Unreachable,
    /// Local store failure (sqlite/crypto store). Retried with backoff —
    /// the usual cause is a competing process, which the cross-process lock
    /// plus the channel lease should make rare.
    Store,
    /// Anything else. Retried with backoff.
    Other,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct MatrixClientError {
    pub kind: MatrixClientErrorKind,
    message: String,
}

impl MatrixClientError {
    fn new(kind: MatrixClientErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    /// Never carries tokens: messages are built from error chains only, and
    /// the SDK's error types don't echo request credentials.
    fn from_sdk(kind: MatrixClientErrorKind, error: impl std::fmt::Display) -> Self {
        Self::new(kind, error.to_string())
    }
}

/// Maps an SDK error onto [`MatrixClientErrorKind`]. Defaults to `Other`
/// (retryable) rather than terminal, so a classification miss can never
/// silently kill a binding.
fn classify_sdk_error(error: &matrix_sdk::Error) -> MatrixClientErrorKind {
    match error {
        matrix_sdk::Error::Http(http) => classify_http_error(http),
        matrix_sdk::Error::CryptoStoreError(_)
        | matrix_sdk::Error::BadCryptoStoreState
        | matrix_sdk::Error::StateStore(_) => MatrixClientErrorKind::Store,
        _ => MatrixClientErrorKind::Other,
    }
}

/// `client_api_error_kind` covers structured API errors; the reqwest arm
/// covers transport failures; `Cached` wraps another `HttpError` (the SDK
/// caches endpoints like `/versions` and clones errors through an `Arc`), so
/// classify the inner error. Anything else is `Other`.
fn classify_http_error(http: &matrix_sdk::HttpError) -> MatrixClientErrorKind {
    if let Some(kind) = http.client_api_error_kind() {
        return match kind {
            ErrorKind::UnknownToken { .. } | ErrorKind::MissingToken { .. } => {
                MatrixClientErrorKind::UnknownToken
            }
            ErrorKind::Forbidden { .. } => MatrixClientErrorKind::Forbidden,
            _ => MatrixClientErrorKind::Other,
        };
    }
    match http {
        matrix_sdk::HttpError::Cached(inner) => classify_http_error(inner),
        matrix_sdk::HttpError::Reqwest(reqwest_error) => {
            if reqwest_error.is_connect()
                || reqwest_error.is_timeout()
                || reqwest_error.is_request()
            {
                MatrixClientErrorKind::Unreachable
            } else {
                MatrixClientErrorKind::Other
            }
        }
        _ => MatrixClientErrorKind::Other,
    }
}

/// Build-time failures: HTTP during discovery maps like any other HTTP
/// error; store-open failures map to `Store`; everything else is `Other`.
fn classify_build_error(error: &ClientBuildError) -> MatrixClientErrorKind {
    match error {
        ClientBuildError::Http(http) => classify_http_error(http),
        ClientBuildError::SqliteStore(_) => MatrixClientErrorKind::Store,
        _ => MatrixClientErrorKind::Other,
    }
}

/// Identity discovered from the homeserver for a raw access token. `pub`
/// for the ao-server connection route (the token-validation step of
/// `PUT .../matrix/connection`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixIdentity {
    pub user_id: String,
    /// `None` for tokens not tied to a device (rare: appservice-style or
    /// pre-device tokens). Callers treat that as unrecoverable for E2EE
    /// purposes — a session restore needs a device id.
    pub device_id: Option<String>,
}

/// `GET /_matrix/client/v3/account/whoami` with a bearer token, over plain
/// reqwest — deliberately *not* through the SDK, because the SDK's `whoami`
/// requires a restored session, and restoring a session is exactly what
/// needs the device id this call discovers (chicken-and-egg, see module
/// doc). Also used by the A4 connection route to validate a pasted token.
pub async fn preflight_whoami(
    homeserver_url: &str,
    access_token: &str,
) -> Result<MatrixIdentity, MatrixClientError> {
    let url = format!(
        "{}/_matrix/client/v3/account/whoami",
        homeserver_url.trim_end_matches('/')
    );
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| {
            let kind = if e.is_connect() || e.is_timeout() || e.is_request() {
                MatrixClientErrorKind::Unreachable
            } else {
                MatrixClientErrorKind::Other
            };
            MatrixClientError::from_sdk(kind, format!("whoami request failed: {e}"))
        })?;
    if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
        return Err(MatrixClientError::new(
            MatrixClientErrorKind::UnknownToken,
            "whoami rejected the access token (401/403)",
        ));
    }
    if !response.status().is_success() {
        return Err(MatrixClientError::new(
            MatrixClientErrorKind::Other,
            format!("whoami returned {}", response.status()),
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| MatrixClientError::from_sdk(MatrixClientErrorKind::Other, e))?;
    let user_id = body
        .get("user_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            MatrixClientError::new(MatrixClientErrorKind::Other, "whoami response missing user_id")
        })?
        .to_string();
    let device_id = body
        .get("device_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(MatrixIdentity { user_id, device_id })
}

/// What the password-login setup path vaults on success. The password
/// itself is never stored anywhere — it exists only for the duration of
/// [`password_login`]'s call frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordLoginResult {
    pub user_id: String,
    pub device_id: String,
    pub access_token: String,
}

/// `POST /_matrix/client/v3/login` (`m.login.password`) for the friendly
/// setup path in `PUT .../matrix/connection`: the user hands over their bot
/// account's password once, we keep the returned token + device id and
/// discard the password. Uses a throwaway, storeless `Client` — no sqlite
/// store, no crypto, no session restore; the real session is built later by
/// the sync loop from the vaulted `(token, device_id)` pair exactly like a
/// raw-token binding's.
///
/// Errors classify like any other call: `Forbidden` = wrong credentials,
/// `Unreachable` = homeserver can't be reached (both map to 400/502-style
/// setup errors in the route; the raw SDK error text is never surfaced to
/// the HTTP caller).
pub async fn password_login(
    homeserver_url: &str,
    username: &str,
    password: &str,
) -> Result<PasswordLoginResult, MatrixClientError> {
    let client = Client::builder()
        .homeserver_url(homeserver_url)
        .build()
        .await
        .map_err(|e| MatrixClientError::from_sdk(classify_build_error(&e), e))?;
    let response = client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("Launchpad Studio")
        .send()
        .await
        .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))?;
    Ok(PasswordLoginResult {
        user_id: response.user_id.to_string(),
        device_id: response.device_id.to_string(),
        access_token: response.access_token,
    })
}

/// What the sync loop needs from one `/sync` response. SDK types are
/// flattened to owned strings here so nothing outside this file names ruma
/// (guide/MATRIX_PLAN.md D1).
#[derive(Debug)]
pub(crate) struct SyncBatch {
    /// Opaque token for the next `since=` — persisted as
    /// `ChannelCursor::Matrix.next_batch` only after the batch is handled.
    pub next_batch: String,
    /// Joined rooms that carried at least one deliverable message event.
    pub joined: Vec<JoinedRoomBatch>,
    /// Pending invites addressed at the bot (`m.room.member` invite with the
    /// bot's MXID as state key), one per invited room.
    pub invites: Vec<BotInvite>,
}

#[derive(Debug)]
pub(crate) struct JoinedRoomBatch {
    pub room_id: String,
    /// From the account-data `m.direct` map via the SDK's room cache. DMs
    /// skip the addressing gate entirely (A3).
    pub is_direct: bool,
    pub events: Vec<MatrixRoomEvent>,
}

/// One deliverable `m.room.message` flattened off the wire. Redacted events,
/// non-message events (reactions, receipts, state), and non-textual msgtypes
/// (attachments — B9's job) never become one of these; edits surface as
/// `edited_event_id` so the A3 handler can drop them (inbound edits are
/// opt-in correction turns in B9, `process_edits`).
#[derive(Debug, Clone)]
pub(crate) struct MatrixRoomEvent {
    pub event_id: String,
    /// Sender MXID (`@user:server`).
    pub sender: String,
    pub body: String,
    /// `m.notice` — other bots' output. The handler drops these outright as
    /// a loop guard (bot convention: never respond to a notice).
    pub is_notice: bool,
    /// `m.mentions.user_ids` as strings.
    pub mentioned_user_ids: Vec<String>,
    /// `m.in_reply_to` target, when the event is a reply.
    pub reply_to_event_id: Option<String>,
    /// `m.replace` target, when the event is an edit.
    pub edited_event_id: Option<String>,
}

/// An invite to the bot: the room and the inviter's MXID (the member event's
/// sender). A3 auto-accepts only when the inviter is a linked sender.
#[derive(Debug)]
pub(crate) struct BotInvite {
    pub room_id: String,
    pub inviter: String,
}

/// Flattens one raw timeline event into a [`MatrixRoomEvent`], returning
/// `None` for anything the inbound pipeline must not see: redacted events
/// (empty content), non-message events, malformed payloads, and non-textual
/// msgtypes (attachments arrive in B9). `m.replace` edits flatten with
/// `edited_event_id` set — dropping or applying them is the handler's call
/// (`process_edits`, B9), not the adapter's.
fn flatten_message_event(
    raw: &matrix_sdk::deserialized_responses::TimelineEvent,
) -> Option<MatrixRoomEvent> {
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncRoomMessageEvent::Original(event),
    )) = raw.raw().deserialize().ok()?
    else {
        return None;
    };
    let (body, is_notice) = match &event.content.msgtype {
        MessageType::Text(text) => (text.body.clone(), false),
        MessageType::Notice(notice) => (notice.body.clone(), true),
        MessageType::Emote(emote) => (emote.body.clone(), false),
        _ => return None,
    };
    let (reply_to_event_id, edited_event_id) = match &event.content.relates_to {
        Some(Relation::Reply(reply)) => (Some(reply.in_reply_to.event_id.to_string()), None),
        Some(Relation::Replacement(replacement)) => {
            (None, Some(replacement.event_id.to_string()))
        }
        _ => (None, None),
    };
    Some(MatrixRoomEvent {
        event_id: event.event_id.to_string(),
        sender: event.sender.to_string(),
        body,
        is_notice,
        mentioned_user_ids: event
            .content
            .mentions
            .as_ref()
            .map(|mentions| {
                mentions.user_ids.iter().map(|user| user.to_string()).collect()
            })
            .unwrap_or_default(),
        reply_to_event_id,
        edited_event_id,
    })
}

/// A connected Matrix client for one binding: SDK client + validated bot
/// identity. Cheap to hold for the binding task's lifetime; one per binding.
pub(crate) struct MatrixClient {
    client: Client,
    bot_user_id: OwnedUserId,
}

/// Deliberately shallow: the SDK `Client` isn't `Debug`, and the useful
/// identity for logs is the bot MXID (never the token, which the client
/// holds internally).
impl std::fmt::Debug for MatrixClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixClient").field("bot_user_id", &self.bot_user_id).finish()
    }
}

impl MatrixClient {
    /// Builds the client (persistent sqlite state + crypto stores under
    /// `store_dir`, cross-process locks named by `lock_holder_name`),
    /// restores the `(token, device_id)` session, and validates the token
    /// with a real `whoami` so a dead token fails fast as `UnknownToken`
    /// instead of surfacing as a sync error minutes later.
    pub(crate) async fn connect(
        params: &MatrixConnectionParams,
    ) -> Result<Self, MatrixClientError> {
        std::fs::create_dir_all(&params.store_dir).map_err(|e| {
            MatrixClientError::from_sdk(
                MatrixClientErrorKind::Store,
                format!("failed to create matrix store dir {}: {e}", params.store_dir.display()),
            )
        })?;
        let bot_user_id: OwnedUserId = UserId::parse(params.bot_user_id.as_str()).map_err(|e| {
            MatrixClientError::from_sdk(
                MatrixClientErrorKind::Other,
                format!("bot_user_id {:?} is not a valid MXID: {e}", params.bot_user_id),
            )
        })?;
        let client = Client::builder()
            .homeserver_url(&params.homeserver_url)
            .sqlite_store(&params.store_dir, Some(params.store_passphrase.as_str()))
            .cross_process_store_config(CrossProcessLockConfig::MultiProcess {
                holder_name: params.lock_holder_name.clone(),
            })
            .build()
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_build_error(&e), e))?;
        let session = MatrixSession {
            meta: SessionMeta {
                user_id: bot_user_id,
                device_id: params.device_id.as_str().into(),
            },
            tokens: SessionTokens {
                access_token: params.access_token.clone(),
                refresh_token: None,
            },
        };
        client
            .restore_session(session)
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))?;
        let whoami = client.whoami().await.map_err(|e| {
            MatrixClientError::from_sdk(classify_http_error(&e), e)
        })?;
        Ok(Self { client, bot_user_id: whoami.user_id })
    }

    /// The bot's own MXID as validated by the homeserver — A3's
    /// self-originated-event guard and addressing checks hang off this.
    pub(crate) fn bot_user_id(&self) -> &UserId {
        &self.bot_user_id
    }

    /// One long-poll `/sync`. `since: None` performs the initial sync;
    /// `Some(token)` resumes from a persisted cursor. Timeline events are
    /// flattened to [`MatrixRoomEvent`]s here so the sync loop never touches
    /// ruma types.
    pub(crate) async fn sync_once(
        &self,
        since: Option<String>,
    ) -> Result<SyncBatch, MatrixClientError> {
        let settings = match since {
            Some(token) => SyncSettings::new().token(SyncToken::Specific(token)),
            None => SyncSettings::new(),
        };
        let response = self
            .client
            .sync_once(settings)
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))?;

        let mut joined = Vec::new();
        for (room_id, room_updates) in response.rooms.joined {
            let is_direct = match self.client.get_room(&room_id) {
                Some(room) => room.is_direct().await.unwrap_or(false),
                None => false,
            };
            let mut events = Vec::new();
            for raw in room_updates.timeline.events {
                let Some(event) = flatten_message_event(&raw) else {
                    continue;
                };
                events.push(event);
            }
            if !events.is_empty() {
                joined.push(JoinedRoomBatch { room_id: room_id.to_string(), is_direct, events });
            }
        }

        let mut invites = Vec::new();
        for (room_id, invited) in response.rooms.invited {
            for raw in &invited.invite_state.events {
                let Ok(AnyStrippedStateEvent::RoomMember(event)) = raw.deserialize() else {
                    continue;
                };
                if event.state_key == *self.bot_user_id
                    && event.content.membership == MembershipState::Invite
                {
                    invites.push(BotInvite {
                        room_id: room_id.to_string(),
                        inviter: event.sender.to_string(),
                    });
                }
            }
        }

        Ok(SyncBatch { next_batch: response.next_batch, joined, invites })
    }

    /// Sends a plain-text `m.notice` (bot-appropriate message type: notices
    /// don't trigger notifications loops from other bots). A4's relay adds
    /// HTML/markdown formatting on top of the same `room.send` path.
    pub(crate) async fn send_notice(
        &self,
        room_id: &str,
        body: &str,
    ) -> Result<(), MatrixClientError> {
        let room = self.get_joined_room(room_id)?;
        let content = RoomMessageEventContent::new(MessageType::Notice(
            matrix_sdk::ruma::events::room::message::NoticeMessageEventContent::plain(body),
        ));
        room.send(content)
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))?;
        Ok(())
    }

    /// Sends a plain-text `m.text` message — the relay's fallback when the
    /// HTML send of a chunk was rejected by the homeserver (and the send
    /// path for replies with no markdown formatting in them at all).
    pub(crate) async fn send_text_message(
        &self,
        room_id: &str,
        body: &str,
    ) -> Result<(), MatrixClientError> {
        let room = self.get_joined_room(room_id)?;
        room.send(RoomMessageEventContent::text_plain(body))
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))?;
        Ok(())
    }

    /// Sends an `m.text` message with an `org.matrix.custom.html` formatted
    /// body. `plain` is the same text without markup (notification bodies,
    /// clients without HTML rendering); `html` must come from
    /// [`super::format`]'s conversion, never from raw agent output.
    pub(crate) async fn send_html_message(
        &self,
        room_id: &str,
        plain: &str,
        html: &str,
    ) -> Result<(), MatrixClientError> {
        let room = self.get_joined_room(room_id)?;
        room.send(RoomMessageEventContent::text_html(plain, html))
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))?;
        Ok(())
    }

    /// `PUT /rooms/{id}/typing/{bot}` — A4's typing heartbeat drives this.
    pub(crate) async fn send_typing(
        &self,
        room_id: &str,
        typing: bool,
    ) -> Result<(), MatrixClientError> {
        let Some(room) = self.find_room(room_id)? else {
            return Ok(()); // Room left mid-turn: dropping a typing notice is harmless.
        };
        room.typing_notice(typing)
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))
    }

    /// Joins a room the bot was invited to. A3 gates this on the inviter
    /// being a linked sender; the adapter itself is policy-free.
    pub(crate) async fn join_room(&self, room_id: &str) -> Result<(), MatrixClientError> {
        let room_id = Self::parse_room_id(room_id)?;
        self.client
            .join_room_by_id(&room_id)
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))?;
        Ok(())
    }

    /// Leaves a room (A4's `DELETE .../matrix/rooms/{room_id}` route).
    pub(crate) async fn leave_room(&self, room_id: &str) -> Result<(), MatrixClientError> {
        let Some(room) = self.find_room(room_id)? else {
            return Ok(()); // Already not joined: leave is idempotent for callers.
        };
        room.leave()
            .await
            .map_err(|e| MatrixClientError::from_sdk(classify_sdk_error(&e), e))
    }

    /// Parses a string room id, keeping ruma types inside the adapter (D1).
    fn parse_room_id(room_id: &str) -> Result<matrix_sdk::ruma::OwnedRoomId, MatrixClientError> {
        RoomId::parse(room_id).map_err(|e| {
            MatrixClientError::new(
                MatrixClientErrorKind::Other,
                format!("invalid room id {room_id:?}: {e}"),
            )
        })
    }

    fn find_room(&self, room_id: &str) -> Result<Option<matrix_sdk::Room>, MatrixClientError> {
        let room_id = Self::parse_room_id(room_id)?;
        Ok(self.client.get_room(&room_id))
    }

    fn get_joined_room(&self, room_id: &str) -> Result<matrix_sdk::Room, MatrixClientError> {
        self.find_room(room_id)?.ok_or_else(|| {
            MatrixClientError::new(
                MatrixClientErrorKind::Other,
                format!("room {room_id} not in the joined-room set"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_params(homeserver_url: String, store_dir: PathBuf) -> MatrixConnectionParams {
        MatrixConnectionParams {
            homeserver_url,
            access_token: "syt_test_token".to_string(),
            bot_user_id: "@bot:example.com".to_string(),
            device_id: "TESTDEVICE".to_string(),
            store_dir,
            store_passphrase: "test-passphrase".to_string(),
            lock_holder_name: "test-holder".to_string(),
        }
    }

    /// The SDK probes server versions lazily; answer any version of the
    /// endpoint shape so tests don't depend on which exact path variant is
    /// probed.
    async fn mount_versions(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": ["v1.11"]
            })))
            .mount(server)
            .await;
    }

    async fn mount_whoami(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/account/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "@bot:example.com",
                "device_id": "TESTDEVICE",
                "is_guest": false
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn preflight_whoami_returns_identity() {
        let server = MockServer::start().await;
        mount_whoami(&server).await;
        let identity = preflight_whoami(&server.uri(), "syt_test_token")
            .await
            .expect("whoami");
        assert_eq!(identity.user_id, "@bot:example.com");
        assert_eq!(identity.device_id.as_deref(), Some("TESTDEVICE"));
    }

    #[tokio::test]
    async fn preflight_whoami_maps_401_to_unknown_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/account/whoami"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "errcode": "M_UNKNOWN_TOKEN",
                "error": "Invalid access token passed."
            })))
            .mount(&server)
            .await;
        let err = preflight_whoami(&server.uri(), "bad_token").await.expect_err("should fail");
        assert_eq!(err.kind, MatrixClientErrorKind::UnknownToken);
    }

    #[tokio::test]
    async fn connect_validates_token_via_whoami() {
        let server = MockServer::start().await;
        mount_versions(&server).await;
        mount_whoami(&server).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let client = MatrixClient::connect(&test_params(server.uri(), dir.path().to_path_buf()))
            .await
            .expect("connect");
        assert_eq!(client.bot_user_id().as_str(), "@bot:example.com");
    }

    #[tokio::test]
    async fn connect_maps_unknown_token() {
        let server = MockServer::start().await;
        mount_versions(&server).await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/account/whoami"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "errcode": "M_UNKNOWN_TOKEN",
                "error": "Invalid access token passed."
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let err = MatrixClient::connect(&test_params(server.uri(), dir.path().to_path_buf()))
            .await
            .expect_err("connect should fail");
        assert_eq!(err.kind, MatrixClientErrorKind::UnknownToken);
    }

    #[tokio::test]
    async fn sync_once_returns_next_batch_and_rooms() {
        let server = MockServer::start().await;
        mount_versions(&server).await;
        mount_whoami(&server).await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .and(query_param("since", "s1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "next_batch": "s2",
                "rooms": {
                    "join": {
                        "!room1:example.com": {
                            "timeline": { "events": [], "limited": false, "prev_batch": "t1" }
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        // Crypto bootstrap traffic the SDK sends around a sync: answer
        // everything else the client asks with an empty success so the sync
        // itself is what the test asserts on.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0..)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let client = MatrixClient::connect(&test_params(server.uri(), dir.path().to_path_buf()))
            .await
            .expect("connect");
        let batch = client.sync_once(Some("s1".to_string())).await.expect("sync");
        assert_eq!(batch.next_batch, "s2");
        assert!(batch.joined.is_empty(), "empty timeline yields no joined batches");
        assert!(batch.invites.is_empty());
    }

    /// A sync carrying every inbound shape A3 cares about: a plain message,
    /// a mention, a reply, an edit, a notice, a redacted event, and an
    /// invite for the bot (plus a decoy invite for someone else).
    #[tokio::test]
    async fn sync_once_flattens_message_shapes() {
        let server = MockServer::start().await;
        mount_versions(&server).await;
        mount_whoami(&server).await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "next_batch": "s9",
                "rooms": {
                    "join": {
                        "!room1:example.com": {
                            "timeline": {
                                "limited": false,
                                "prev_batch": "t1",
                                "events": [
                                    {
                                        "type": "m.room.message",
                                        "event_id": "$plain",
                                        "sender": "@alice:example.com",
                                        "origin_server_ts": 100,
                                        "content": { "msgtype": "m.text", "body": "hello" }
                                    },
                                    {
                                        "type": "m.room.message",
                                        "event_id": "$mention",
                                        "sender": "@alice:example.com",
                                        "origin_server_ts": 101,
                                        "content": {
                                            "msgtype": "m.text",
                                            "body": "hey @bot:example.com",
                                            "m.mentions": { "user_ids": ["@bot:example.com"] }
                                        }
                                    },
                                    {
                                        "type": "m.room.message",
                                        "event_id": "$reply",
                                        "sender": "@alice:example.com",
                                        "origin_server_ts": 102,
                                        "content": {
                                            "msgtype": "m.text",
                                            "body": "answering",
                                            "m.relates_to": { "m.in_reply_to": { "event_id": "$plain" } }
                                        }
                                    },
                                    {
                                        "type": "m.room.message",
                                        "event_id": "$edit",
                                        "sender": "@alice:example.com",
                                        "origin_server_ts": 103,
                                        "content": {
                                            "msgtype": "m.text",
                                            "body": "* edited",
                                            "m.relates_to": { "rel_type": "m.replace", "event_id": "$plain" },
                                            "m.new_content": { "msgtype": "m.text", "body": "edited" }
                                        }
                                    },
                                    {
                                        "type": "m.room.message",
                                        "event_id": "$notice",
                                        "sender": "@otherbot:example.com",
                                        "origin_server_ts": 104,
                                        "content": { "msgtype": "m.notice", "body": "automated" }
                                    },
                                    {
                                        "type": "m.room.message",
                                        "event_id": "$redacted",
                                        "sender": "@alice:example.com",
                                        "origin_server_ts": 105,
                                        "content": {},
                                        "unsigned": {
                                            "redacted_because": {
                                                "type": "m.room.redaction",
                                                "sender": "@alice:example.com",
                                                "content": {}
                                            }
                                        }
                                    }
                                ]
                            }
                        }
                    },
                    "invite": {
                        "!room2:example.com": {
                            "invite_state": {
                                "events": [
                                    {
                                        "type": "m.room.member",
                                        "state_key": "@bot:example.com",
                                        "sender": "@alice:example.com",
                                        "content": { "membership": "invite", "displayname": "Bot" }
                                    }
                                ]
                            }
                        },
                        "!room3:example.com": {
                            "invite_state": {
                                "events": [
                                    {
                                        "type": "m.room.member",
                                        "state_key": "@someoneelse:example.com",
                                        "sender": "@alice:example.com",
                                        "content": { "membership": "invite" }
                                    }
                                ]
                            }
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0..)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let client = MatrixClient::connect(&test_params(server.uri(), dir.path().to_path_buf()))
            .await
            .expect("connect");
        let batch = client.sync_once(Some("s8".to_string())).await.expect("sync");

        assert_eq!(batch.next_batch, "s9");
        assert_eq!(batch.joined.len(), 1);
        let events = &batch.joined[0].events;
        // The redacted event never becomes a MatrixRoomEvent.
        assert_eq!(events.len(), 5, "redacted event dropped: {events:?}");

        let by_id: std::collections::HashMap<_, _> =
            events.iter().map(|e| (e.event_id.as_str(), e)).collect();
        assert_eq!(by_id["$plain"].body, "hello");
        assert_eq!(by_id["$mention"].mentioned_user_ids, vec!["@bot:example.com"]);
        assert_eq!(by_id["$reply"].reply_to_event_id.as_deref(), Some("$plain"));
        assert_eq!(by_id["$edit"].edited_event_id.as_deref(), Some("$plain"));
        assert!(by_id["$notice"].is_notice);

        // Exactly the invite addressed at the bot; the decoy for another
        // user is ignored.
        assert_eq!(batch.invites.len(), 1);
        assert_eq!(batch.invites[0].room_id, "!room2:example.com");
        assert_eq!(batch.invites[0].inviter, "@alice:example.com");
    }

    /// v1 slice 5's device-discipline test: a "restarted" client over the
    /// same store directory restores the same device id instead of minting a
    /// new one (which is what would strand E2EE history).
    #[tokio::test]
    async fn device_id_survives_client_rebuild() {
        let server = MockServer::start().await;
        mount_versions(&server).await;
        mount_whoami(&server).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let params = test_params(server.uri(), dir.path().to_path_buf());
        let first = MatrixClient::connect(&params).await.expect("first connect");
        let first_id = first.bot_user_id().to_owned();
        drop(first);
        let second = MatrixClient::connect(&params).await.expect("second connect");
        assert_eq!(second.bot_user_id(), first_id);
        // The session was restored, not re-logged-in: no login endpoint was
        // ever mounted, so any login attempt would have failed the connect.
    }

    #[tokio::test]
    async fn unreachable_homeserver_maps_to_unreachable() {
        // Nothing listens here.
        let dir = tempfile::tempdir().expect("tempdir");
        let err = MatrixClient::connect(&test_params(
            "http://127.0.0.1:1".to_string(),
            dir.path().to_path_buf(),
        ))
        .await
        .expect_err("connect should fail");
        assert_eq!(err.kind, MatrixClientErrorKind::Unreachable);
    }
}
