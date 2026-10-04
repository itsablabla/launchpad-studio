//! Outbound half of the Matrix bridge (A4).
//!
//! Complements [`super::transport`]'s sync loop with the reverse path: when
//! an agent finishes a turn that was triggered by an inbound Matrix message,
//! relay its final reply back to the originating room. Mirrors
//! [`crate::telegram::outbound`] structurally — one shared `EventBus`
//! observer, the last `TextComplete` per thread flushed at `RunEnded`
//! through the shared [`handle_relay_event`] step — with two Matrix-shaped
//! differences:
//!
//! - The correlation value is a [`MatrixReplyTarget`] (`room_id` +
//!   `trigger_event_id` from the start, so B8's ack reactions need no map
//!   widening), and the send itself goes through the *sync task's* live
//!   [`MatrixClient`] (looked up from [`MatrixTransport`]'s client
//!   registry): `room.send` encrypts automatically for E2EE rooms, which a
//!   bare HTTP `PUT /send` would silently not do.
//! - The typing heartbeat pings `PUT /rooms/{id}/typing/{bot}` on a ~10s
//!   cadence (Matrix typing notifications carry a server-side timeout, ~30s
//!   by default — no sub-5s pinging like Telegram needs).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use ao_persistence::PersistenceLayer;
use ao_protocol::event::{AgentEvent, AgentEventPayload};

use crate::channels::relay::lease_gate::LeaseGate;
use crate::channels::relay::observer::{handle_relay_event, recover_lagged_replies, RelaySink};
use crate::event_bus::EventBus;

use super::client::MatrixClient;
use super::format::{chunk_for_matrix, markdown_to_matrix_html};
use super::transport::MatrixTransport;

/// How often the typing heartbeat re-pings while a turn runs. The
/// homeserver expires a typing notification ~30s after the last ping
/// (spec default timeout), so 10s keeps the indicator alive with generous
/// margin without spamming.
const TYPING_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Where a bridge thread's reply goes, recorded by the inbound pipeline
/// right before dispatch. `room_id` is the delivery address; `binding_id` +
/// `agent_id` resolve which live [`MatrixClient`] (and thereby which bot
/// account) sends it; `trigger_event_id` is the inbound event that started
/// the turn — carried from day one (plan §5: "wide from the start") so B8's
/// ack reactions (👀 on dispatch, ✅/⚠️ on completion) need no correlation
/// map widening later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatrixReplyTarget {
    pub agent_id: String,
    pub binding_id: String,
    pub room_id: String,
    pub trigger_event_id: String,
}

/// Runs until `shutdown_rx` fires. One subscription for the whole process —
/// every agent's events flow through it; only threads the transport's
/// correlation map is currently tracking ever trigger a relay. Structurally
/// identical to Telegram's `run_outbound_observer` (the per-channel outer
/// loop exists so the typing heartbeat lifecycle can hook `RunStarted` /
/// `RunEnded`, which the shared relay step doesn't model).
pub(crate) async fn run_outbound_observer(
    transport: Arc<MatrixTransport>,
    persistence: Arc<PersistenceLayer>,
    lease_gate: Arc<LeaseGate>,
    event_bus: Arc<EventBus>,
    mut shutdown_rx: watch::Receiver<()>,
) {
    let mut events = event_bus.subscribe();
    // thread_id -> latest TextComplete text seen since that thread's run started.
    let mut pending_text: HashMap<String, String> = HashMap::new();
    // thread_id -> text of the last reply actually relayed for that thread —
    // see `recover_lagged_replies` for why this is kept alongside `pending_text`.
    let mut last_relayed: HashMap<String, String> = HashMap::new();
    // thread_id -> cancel signal for that thread's in-flight typing heartbeat.
    let mut heartbeats: HashMap<String, CancellationToken> = HashMap::new();

    info!("MatrixBridge outbound observer starting");

    loop {
        let event = tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("MatrixBridge outbound observer shutting down");
                for cancel in heartbeats.values() {
                    cancel.cancel();
                }
                return;
            }
            event = events.recv() => event,
        };

        let event = match event {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                recover_lagged_replies(
                    lease_gate.as_ref(),
                    persistence.as_ref(),
                    transport.in_flight(),
                    transport.as_ref(),
                    &mut last_relayed,
                    skipped,
                )
                .await;
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };

        handle_event(&transport, &lease_gate, event, &mut pending_text, &mut last_relayed, &mut heartbeats).await;
    }
}

/// Processes one event from the shared bus: heartbeat lifecycle on
/// `RunStarted`/`RunEnded` (Matrix-only side effect), then the shared relay
/// step (TextComplete buffering + RunEnded resolve-and-relay). Split out
/// from [`run_outbound_observer`] so tests drive it with synthetic events.
async fn handle_event(
    transport: &MatrixTransport,
    lease_gate: &LeaseGate,
    event: AgentEvent,
    pending_text: &mut HashMap<String, String>,
    last_relayed: &mut HashMap<String, String>,
    heartbeats: &mut HashMap<String, CancellationToken>,
) {
    let Some(thread_id) = event.thread_id.clone() else {
        return;
    };

    match &event.payload {
        AgentEventPayload::RunStarted => {
            start_typing_heartbeat(transport, &event.agent_id, &thread_id, heartbeats);
        }
        AgentEventPayload::RunEnded { .. } => {
            // Stop this thread's heartbeat on completion whether or not the
            // run turns out to be Matrix-triggered — mirrors the shared
            // observer's unconditional `pending_text` cleanup; a thread
            // that never matched the correlation map must not leak state.
            if let Some(cancel) = heartbeats.remove(&thread_id) {
                cancel.cancel();
                debug!(thread_id = %thread_id, "matrix heartbeat: cancelled on run end");
            }
        }
        _ => {}
    }

    handle_relay_event(lease_gate, transport.in_flight(), transport, event, pending_text, last_relayed).await;
}

#[async_trait]
impl RelaySink<MatrixReplyTarget> for MatrixTransport {
    async fn relay(&self, agent_id: &str, origin: &MatrixReplyTarget, text: &str) {
        relay_reply(self, agent_id, origin, text).await;
    }
}

/// Starts a typing-heartbeat task for `thread_id` if it's actually
/// Matrix-correlated (the inbound pipeline recorded a [`MatrixReplyTarget`]
/// for it before dispatch) and the binding's sync task currently holds a
/// connected client. Reads the mapping with the non-consuming `peek`, same
/// as `RunEnded`'s relay does.
fn start_typing_heartbeat(
    transport: &MatrixTransport,
    agent_id: &str,
    thread_id: &str,
    heartbeats: &mut HashMap<String, CancellationToken>,
) {
    let Some(target) = transport.in_flight().peek(thread_id) else {
        return;
    };
    let Some(client) = transport.client_for(&target.agent_id, &target.binding_id) else {
        debug!(thread_id = %thread_id, agent_id = %agent_id, "MatrixBridge: binding has no connected client, skipping typing heartbeat");
        return;
    };

    let cancel = CancellationToken::new();
    tokio::spawn(run_typing_heartbeat(client, target.room_id, agent_id.to_string(), cancel.clone()));
    heartbeats.insert(thread_id.to_string(), cancel);
}

/// Pings the typing endpoint immediately, then every
/// [`TYPING_HEARTBEAT_INTERVAL`] until `cancel` fires (from `RunEnded` or
/// observer shutdown). A failed ping is logged and swallowed — it must never
/// affect the real reply relay, a separate call on a separate path.
async fn run_typing_heartbeat(
    client: Arc<MatrixClient>,
    room_id: String,
    agent_id: String,
    cancel: CancellationToken,
) {
    loop {
        if let Err(e) = client.send_typing(&room_id, true).await {
            warn!(agent_id = %agent_id, room_id = %room_id, "MatrixBridge: failed to send typing heartbeat: {e}");
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(TYPING_HEARTBEAT_INTERVAL) => {}
        }
    }
}

/// Relays `text` to the target room, chunked to [`super::format`]'s limit.
/// Each chunk is converted from the agent's markdown to Matrix's spec-HTML
/// subset and sent with `format: org.matrix.custom.html`; chunks with no
/// markdown constructs go as plain `m.text`. If the homeserver rejects the
/// HTML send (a converter bug producing markup it refuses), that one chunk
/// is retried once as plain text — delivering the reply, even unformatted,
/// always beats dropping it. Every other failure (binding not connected,
/// room left mid-turn, network) is logged and swallowed: a failed relay
/// must never crash the turn, the thread, or the process (the `RelaySink`
/// contract).
async fn relay_reply(
    transport: &MatrixTransport,
    agent_id: &str,
    target: &MatrixReplyTarget,
    text: &str,
) {
    let Some(client) = transport.client_for(agent_id, &target.binding_id) else {
        warn!(
            agent_id = %agent_id,
            binding_id = %target.binding_id,
            room_id = %target.room_id,
            "MatrixBridge: no connected client for binding, dropping relay"
        );
        return;
    };

    for chunk in chunk_for_matrix(text) {
        match markdown_to_matrix_html(chunk) {
            Some(html) => {
                if let Err(e) = client.send_html_message(&target.room_id, chunk, &html).await {
                    warn!(
                        agent_id = %agent_id,
                        room_id = %target.room_id,
                        "MatrixBridge: HTML send failed, retrying this chunk as plain text: {e}"
                    );
                    if let Err(e) = client.send_text_message(&target.room_id, chunk).await {
                        warn!(
                            agent_id = %agent_id,
                            room_id = %target.room_id,
                            "MatrixBridge: plain-text fallback also failed to relay reply: {e}"
                        );
                        return;
                    }
                }
            }
            None => {
                if let Err(e) = client.send_text_message(&target.room_id, chunk).await {
                    warn!(
                        agent_id = %agent_id,
                        room_id = %target.room_id,
                        "MatrixBridge: failed to relay reply: {e}"
                    );
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;
    use uuid::Uuid;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use ao_protocol::event::RunEndReason;

    use super::super::client::{MatrixClient, MatrixConnectionParams};
    use crate::channels::relay::observer::RUN_FAILED_NOTICE;

    fn make_event(agent_id: &str, thread_id: &str, payload: AgentEventPayload) -> AgentEvent {
        AgentEvent {
            event_id: Uuid::new_v4().to_string(),
            run_id: format!("run-{}", Uuid::new_v4()),
            seq: 0,
            ts: Utc::now(),
            agent_id: agent_id.to_string(),
            thread_id: Some(thread_id.to_string()),
            payload,
        }
    }

    fn target(agent_id: &str, room_id: &str) -> MatrixReplyTarget {
        MatrixReplyTarget {
            agent_id: agent_id.to_string(),
            binding_id: "matrix".to_string(),
            room_id: room_id.to_string(),
            trigger_event_id: "$trigger".to_string(),
        }
    }

    /// A connected `MatrixClient` against the wiremock homeserver, synced
    /// once so `!room1:example.com` is in its joined-room set (the adapter's
    /// send methods require a known room, like any `Room::send` caller).
    async fn connected_transport(
        server: &MockServer,
    ) -> (MatrixTransport, tempfile::TempDir) {
        Mock::given(method("GET"))
            .and(path_regex("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": ["v1.8"]
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/account/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "@bot:example.com",
                "device_id": "DEVTEST"
            })))
            .mount(server)
            .await;
        // Room-join sync so the room is known; then every send/typing
        // endpoint succeeds.
        Mock::given(method("GET"))
            .and(path_regex("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "next_batch": "s1",
                "rooms": {
                    "join": {
                        "!room1:example.com": {
                            "timeline": { "events": [], "limited": false, "prev_batch": "t1" }
                        }
                    }
                }
            })))
            .mount(server)
            .await;
        // `Room::send` checks the room's encryption state first (the client
        // has a crypto store, so E2EE is active). A spec-shaped M_NOT_FOUND
        // marks the room unencrypted and the plaintext PUT proceeds.
        Mock::given(method("GET"))
            .and(path_regex("/_matrix/client/v3/rooms/!room1:example.com/state/m.room.encryption/"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "errcode": "M_NOT_FOUND",
                "error": "Event not found"
            })))
            .mount(server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "event_id": "$sent"
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0..)
            .mount(server)
            .await;

        let dir = tempfile::tempdir().expect("tempdir");
        let params = MatrixConnectionParams {
            homeserver_url: server.uri(),
            access_token: "test-token".to_string(),
            bot_user_id: "@bot:example.com".to_string(),
            device_id: "DEVTEST".to_string(),
            store_dir: dir.path().to_path_buf(),
            store_passphrase: "test-passphrase".to_string(),
            lock_holder_name: "matrix-outbound-test".to_string(),
        };
        let client = MatrixClient::connect(&params).await.expect("connect");
        client.sync_once(Some("s0".to_string())).await.expect("sync joins the room");

        let transport = MatrixTransport::new();
        transport.register_client_for_tests("agent-x", "matrix", Arc::new(client));
        (transport, dir)
    }

    /// Every send the wiremock server received, as parsed JSON bodies.
    async fn sent_messages(server: &MockServer) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .expect("requests recorded")
            .into_iter()
            .filter(|req| req.url.path().contains("/send/m.room.message/"))
            .map(|req| serde_json::from_slice(&req.body).expect("send body is json"))
            .collect()
    }

    #[tokio::test]
    async fn relay_sends_last_text_complete_to_the_recorded_room_on_run_ended() {
        let server = MockServer::start().await;
        let (transport, _dir) = connected_transport(&server).await;
        let lease_gate = LeaseGate::new();
        transport.in_flight().record("thread-1", target("agent-x", "!room1:example.com"));
        lease_gate.mark_active("matrix", "thread-1");
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-1", AgentEventPayload::TextComplete { text: "draft".to_string() }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-1", AgentEventPayload::TextComplete { text: "final **reply**".to_string() }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-1", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        let sends = sent_messages(&server).await;
        assert_eq!(sends.len(), 1, "exactly one relay, with the last buffered text");
        let send = &sends[0];
        assert_eq!(send["msgtype"], "m.text");
        assert_eq!(send["body"], "final **reply**");
        // Markdown was converted: the HTML body carries the formatting.
        assert_eq!(send["format"], "org.matrix.custom.html");
        assert!(
            send["formatted_body"].as_str().unwrap_or_default().contains("<strong>reply</strong>"),
            "{send}"
        );
    }

    #[tokio::test]
    async fn relay_of_plain_text_sends_no_formatted_body() {
        let server = MockServer::start().await;
        let (transport, _dir) = connected_transport(&server).await;
        let lease_gate = LeaseGate::new();
        transport.in_flight().record("thread-plain", target("agent-x", "!room1:example.com"));
        lease_gate.mark_active("matrix", "thread-plain");
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-plain", AgentEventPayload::TextComplete { text: "just words".to_string() }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-plain", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        let sends = sent_messages(&server).await;
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0]["body"], "just words");
        assert!(
            sends[0].get("format").is_none(),
            "unformatted replies must not carry a formatted_body: {}",
            sends[0]
        );
    }

    #[tokio::test]
    async fn run_ended_error_with_no_reply_relays_a_failure_notice_to_the_room() {
        let server = MockServer::start().await;
        let (transport, _dir) = connected_transport(&server).await;
        let lease_gate = LeaseGate::new();
        transport.in_flight().record("thread-err", target("agent-x", "!room1:example.com"));
        lease_gate.mark_active("matrix", "thread-err");
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-err", AgentEventPayload::RunEnded { reason: RunEndReason::Error }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;

        let sends = sent_messages(&server).await;
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0]["body"], RUN_FAILED_NOTICE);
    }

    #[tokio::test]
    async fn relay_without_a_connected_client_drops_without_panicking() {
        // No client registered: the binding is disconnected. The RelaySink
        // contract is log-and-swallow.
        let transport = MatrixTransport::new();
        let lease_gate = LeaseGate::new();
        transport.in_flight().record("thread-orphan", target("agent-x", "!room1:example.com"));
        lease_gate.mark_active("matrix", "thread-orphan");
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-orphan", AgentEventPayload::TextComplete { text: "lost".to_string() }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-orphan", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        // Reaching this line without a panic is the assertion.
    }

    #[tokio::test]
    async fn run_started_spawns_a_typing_heartbeat_that_pings_the_room() {
        let server = MockServer::start().await;
        let (transport, _dir) = connected_transport(&server).await;
        let lease_gate = LeaseGate::new();
        transport.in_flight().record("thread-typing", target("agent-x", "!room1:example.com"));
        lease_gate.mark_active("matrix", "thread-typing");
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-typing", AgentEventPayload::RunStarted),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        assert!(heartbeats.contains_key("thread-typing"), "a correlated thread gets a heartbeat");

        // The heartbeat's first ping fires immediately on spawn.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let typing_pings = server
            .received_requests()
            .await
            .expect("requests recorded")
            .into_iter()
            .filter(|req| req.url.path().contains("/typing/"))
            .count();
        assert!(typing_pings >= 1, "the heartbeat must ping the typing endpoint");

        // RunEnded cancels and removes it.
        let cancel = heartbeats.get("thread-typing").cloned().expect("tracked");
        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "thread-typing", AgentEventPayload::RunEnded { reason: RunEndReason::Completed }),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        assert!(cancel.is_cancelled());
        assert!(!heartbeats.contains_key("thread-typing"));
    }

    #[tokio::test]
    async fn run_started_on_an_uncorrelated_thread_spawns_no_heartbeat() {
        let server = MockServer::start().await;
        let (transport, _dir) = connected_transport(&server).await;
        let lease_gate = LeaseGate::new();
        let mut pending_text = HashMap::new();
        let mut last_relayed = HashMap::new();
        let mut heartbeats = HashMap::new();

        handle_event(
            &transport,
            &lease_gate,
            make_event("agent-x", "main-thread", AgentEventPayload::RunStarted),
            &mut pending_text,
            &mut last_relayed,
            &mut heartbeats,
        )
        .await;
        assert!(heartbeats.is_empty(), "an app-typed turn must never get a typing heartbeat");
    }

    #[test]
    fn reply_target_is_wide_from_the_start() {
        // Pins the A4 shape the B8 widening depends on: room + trigger
        // event + binding identity, all present from the first version.
        let target = target("agent-x", "!room1:example.com");
        assert_eq!(target.trigger_event_id, "$trigger");
        assert_eq!(target.binding_id, "matrix");
    }
}
