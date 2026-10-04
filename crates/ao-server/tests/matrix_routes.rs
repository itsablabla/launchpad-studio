//! Route tests for `routes/matrix.rs` — mirrors `telegram_routes.rs`'s
//! harness (process-wide env vars ⇒ serialized via `ENV_MUTEX`, run with
//! `--test-threads=1`), with wiremock standing in for the homeserver.
//! Unlike Telegram there is no API-base env var: the homeserver URL travels
//! in the request body, so each test points it at its own mock server.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ao_engine::AppState;
use ao_engine_tools_provider_config::channel_secret_store::ChannelSecretStore;
use ao_engine_tools_provider_config::{MATRIX_DEVICE_ID_SECRET_ROLE, MATRIX_TOKEN_SECRET_ROLE};
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{
    AgentProfile, ChannelKind, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_server::routes::build_router;

/// `ChannelSecretStore::open()` and the persistence layer both read
/// process-wide env vars, so tests in this file must not run concurrently
/// with each other. Run with `--test-threads=1`.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_test_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Test Agent {id}"),
        description: "A test agent".to_string(),
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
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: None,
        tools: None,
        env: HashMap::new(),
        max_instances: 1,
        timeout_seconds: 300,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: None,
        enabled_plugins: HashMap::new(),
        runner_mode: Default::default(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        native_provider: None,
        thinking: None,
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        minimal_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
        max_turns: None,
    }
}

async fn setup() -> (axum::Router, Arc<AppState>, tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
    let guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
    std::env::set_var("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");

    let mock = MockProcessSupervisor::new(vec![]);
    let state = Arc::new(AppState::new_with_mock(mock).await.expect("init state"));
    let router = build_router(Arc::clone(&state));
    (router, state, tmp, guard)
}

/// Mounts the one endpoint the token path needs: `GET
/// /_matrix/client/v3/account/whoami` (the adapter's `preflight_whoami`
/// deliberately runs over plain HTTP, so no `/versions` mount is needed).
async fn mount_whoami(server: &MockServer, status: u16, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/_matrix/client/v3/account/whoami"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

/// Mounts `POST /_matrix/client/v3/login` for the password path. The SDK
/// fetches `GET /_matrix/client/versions` before its first request, so that
/// endpoint is mounted too.
async fn mount_login(server: &MockServer, status: u16, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "versions": ["v1.8"]
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/login"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn read_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.expect("read body").to_bytes();
    if bytes.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&bytes).expect("valid JSON body") }
}

async fn create_agent(router: &axum::Router, profile: &AgentProfile) {
    let body = serde_json::to_string(profile).unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("create agent");
    assert_eq!(resp.status(), StatusCode::OK, "agent creation failed");
}

async fn put_connection(router: &axum::Router, agent_id: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/agents/{agent_id}/matrix/connection"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("put connection");
    let status = resp.status();
    (status, read_body(resp).await)
}

fn vault_has_token(agent_id: &str) -> bool {
    ChannelSecretStore::open()
        .expect("open store")
        .get(agent_id, "matrix", MATRIX_TOKEN_SECRET_ROLE)
        .expect("read token")
        .is_some()
}

#[tokio::test]
async fn put_connection_with_token_validates_vaults_and_enables() {
    let (router, state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    mount_whoami(&server, 200, serde_json::json!({
        "user_id": "@bot:example.com",
        "device_id": "DEVLOGIN"
    }))
    .await;
    create_agent(&router, &make_test_profile("agent-m1")).await;

    let (status, body) = put_connection(
        &router,
        "agent-m1",
        serde_json::json!({
            "homeserver_url": format!("{}/", server.uri()), // trailing slash must normalize
            "access_token": "s3cr3t-token"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user_id"], "@bot:example.com");

    let profile = state.persistence.agents.get("agent-m1").await.unwrap().unwrap();
    let binding = profile.channel_of_kind(ChannelKind::Matrix).expect("matrix binding");
    assert!(binding.enabled, "a validated connection enables the binding");
    let ao_protocol::agent::ChannelKindConfig::Matrix { homeserver_url, bot_user_id, .. } =
        &binding.kind_config
    else {
        panic!("expected matrix kind_config");
    };
    assert_eq!(bot_user_id.as_deref(), Some("@bot:example.com"), "identity cached on kind_config");
    assert_eq!(homeserver_url, &server.uri(), "trailing slash trimmed");

    let store = ChannelSecretStore::open().unwrap();
    assert_eq!(
        store.get("agent-m1", "matrix", MATRIX_TOKEN_SECRET_ROLE).unwrap().as_deref(),
        Some("s3cr3t-token"),
        "token vaulted under the token role"
    );
    assert_eq!(
        store.get("agent-m1", "matrix", MATRIX_DEVICE_ID_SECRET_ROLE).unwrap().as_deref(),
        Some("DEVLOGIN"),
        "device id vaulted under its own role"
    );
}

#[tokio::test]
async fn put_connection_rejects_an_invalid_token_without_storing_anything() {
    let (router, state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    mount_whoami(&server, 401, serde_json::json!({"errcode": "M_UNKNOWN_TOKEN"})).await;
    create_agent(&router, &make_test_profile("agent-m2")).await;

    let (status, body) = put_connection(
        &router,
        "agent-m2",
        serde_json::json!({"homeserver_url": server.uri(), "access_token": "bad-token"}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(!vault_has_token("agent-m2"), "a rejected token must never be vaulted");
    let profile = state.persistence.agents.get("agent-m2").await.unwrap().unwrap();
    assert!(
        profile.channel_of_kind(ChannelKind::Matrix).is_none(),
        "a rejected connection must not create or enable a binding"
    );
}

#[tokio::test]
async fn put_connection_rejects_a_deviceless_token() {
    let (router, _state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    mount_whoami(&server, 200, serde_json::json!({"user_id": "@bot:example.com"})).await;
    create_agent(&router, &make_test_profile("agent-m3")).await;

    let (status, body) = put_connection(
        &router,
        "agent-m3",
        serde_json::json!({"homeserver_url": server.uri(), "access_token": "tok"}),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("device"),
        "the error must point at the missing device id: {body}"
    );
    assert!(!vault_has_token("agent-m3"));
}

#[tokio::test]
async fn put_connection_with_password_logs_in_and_vaults_the_session() {
    let (router, _state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    mount_login(&server, 200, serde_json::json!({
        "user_id": "@bot:example.com",
        "access_token": "minted-token",
        "device_id": "DEVPASS"
    }))
    .await;
    create_agent(&router, &make_test_profile("agent-m4")).await;

    let (status, body) = put_connection(
        &router,
        "agent-m4",
        serde_json::json!({
            "homeserver_url": server.uri(),
            "username": "bot",
            "password": "the-password"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user_id"], "@bot:example.com");
    let store = ChannelSecretStore::open().unwrap();
    assert_eq!(
        store.get("agent-m4", "matrix", MATRIX_TOKEN_SECRET_ROLE).unwrap().as_deref(),
        Some("minted-token"),
        "the login-minted token is vaulted — the password is not"
    );
    assert_eq!(
        store.get("agent-m4", "matrix", MATRIX_DEVICE_ID_SECRET_ROLE).unwrap().as_deref(),
        Some("DEVPASS")
    );
}

#[tokio::test]
async fn put_connection_with_wrong_password_is_a_validation_error() {
    let (router, _state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    mount_login(&server, 403, serde_json::json!({
        "errcode": "M_FORBIDDEN",
        "error": "Invalid password"
    }))
    .await;
    create_agent(&router, &make_test_profile("agent-m5")).await;

    let (status, body) = put_connection(
        &router,
        "agent-m5",
        serde_json::json!({
            "homeserver_url": server.uri(),
            "username": "bot",
            "password": "wrong"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid username or password");
    assert!(!vault_has_token("agent-m5"));
}

#[tokio::test]
async fn put_connection_requires_exactly_one_auth_shape() {
    let (router, _state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    create_agent(&router, &make_test_profile("agent-m6")).await;

    for body in [
        serde_json::json!({"homeserver_url": server.uri()}),
        serde_json::json!({"homeserver_url": server.uri(), "access_token": "t", "username": "u", "password": "p"}),
        serde_json::json!({"homeserver_url": server.uri(), "username": "u"}),
    ] {
        let (status, body) = put_connection(&router, "agent-m6", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}

#[tokio::test]
async fn status_never_returns_the_token_and_reports_the_cached_identity() {
    let (router, _state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    mount_whoami(&server, 200, serde_json::json!({
        "user_id": "@bot:example.com",
        "device_id": "DEV1"
    }))
    .await;
    create_agent(&router, &make_test_profile("agent-m7")).await;
    let (status, _) = put_connection(
        &router,
        "agent-m7",
        serde_json::json!({"homeserver_url": server.uri(), "access_token": "s3cr3t-token"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/agent-m7/matrix/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("status");
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = resp.into_body().collect().await.unwrap().to_bytes();
    let raw_text = String::from_utf8(raw.to_vec()).unwrap();
    assert!(!raw_text.contains("s3cr3t-token"), "the token must never appear in a status body: {raw_text}");

    let body: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(body["has_token"], true);
    assert_eq!(body["bot_user_id"], "@bot:example.com");
    assert_eq!(body["enabled"], true);
    assert!(body["connection_state"].is_string(), "live connection state is reported: {body}");
}

#[tokio::test]
async fn pairing_code_requires_a_configured_binding_then_mints() {
    let (router, _state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    mount_whoami(&server, 200, serde_json::json!({
        "user_id": "@bot:example.com",
        "device_id": "DEV1"
    }))
    .await;
    create_agent(&router, &make_test_profile("agent-m8")).await;

    // No connection yet: 400.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-m8/matrix/pairing-code")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Connected: a code is minted and visible in status.
    let (status, _) = put_connection(
        &router,
        "agent-m8",
        serde_json::json!({"homeserver_url": server.uri(), "access_token": "tok"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/agent-m8/matrix/pairing-code")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert!(body["code"].as_str().unwrap().len() >= 6, "{body}");

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/agent-m8/matrix/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status_body = read_body(resp).await;
    assert_eq!(status_body["pending_pairing_code"]["code"], body["code"]);
}

#[tokio::test]
async fn unlinking_a_room_revokes_it_but_keeps_the_pairing_user_linked() {
    let (router, state, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-m9")).await;

    // Seed the store exactly like a successful pairing does (room + MXID).
    let senders = &state.persistence.linked_senders;
    senders.add_sender("agent-m9", "matrix", "!room:example.com").await.unwrap();
    senders.add_sender("agent-m9", "matrix", "@alice:example.com").await.unwrap();

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/agents/agent-m9/matrix/rooms/!room:example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["linked_rooms"], serde_json::json!([]));

    let remaining = senders.get("agent-m9", "matrix").await.unwrap().unwrap().senders;
    assert!(
        !remaining.iter().any(|s| s == "!room:example.com"),
        "the unlinked room must lose its authorization"
    );
    assert!(
        remaining.iter().any(|s| s == "@alice:example.com"),
        "the pairing user's MXID link (invite gate) is separate and survives: {remaining:?}"
    );
}

#[tokio::test]
async fn deleting_the_connection_revokes_secrets_senders_and_disables() {
    let (router, state, _tmp, _guard) = setup().await;
    let server = MockServer::start().await;
    mount_whoami(&server, 200, serde_json::json!({
        "user_id": "@bot:example.com",
        "device_id": "DEV1"
    }))
    .await;
    create_agent(&router, &make_test_profile("agent-m10")).await;
    let (status, _) = put_connection(
        &router,
        "agent-m10",
        serde_json::json!({"homeserver_url": server.uri(), "access_token": "tok"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    state.persistence.linked_senders.add_sender("agent-m10", "matrix", "!room:example.com").await.unwrap();

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/agents/agent-m10/matrix/connection")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(!vault_has_token("agent-m10"), "the vaulted token must be deleted");
    let profile = state.persistence.agents.get("agent-m10").await.unwrap().unwrap();
    let binding = profile.channel_of_kind(ChannelKind::Matrix).expect("binding survives, disabled");
    assert!(!binding.enabled);
    let ao_protocol::agent::ChannelKindConfig::Matrix { bot_user_id, .. } = &binding.kind_config
    else {
        panic!("expected matrix kind_config");
    };
    assert!(bot_user_id.is_none(), "cached identity cleared with the connection");
    let remaining = state
        .persistence
        .linked_senders
        .get("agent-m10", "matrix")
        .await
        .unwrap()
        .unwrap_or_default()
        .senders;
    assert!(remaining.is_empty(), "every linked sender is revoked with the connection");
}

#[tokio::test]
async fn status_for_an_unconfigured_agent_is_empty_and_tokenless() {
    let (router, _state, _tmp, _guard) = setup().await;
    create_agent(&router, &make_test_profile("agent-m11")).await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/agent-m11/matrix/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_body(resp).await;
    assert_eq!(body["has_token"], false);
    assert_eq!(body["enabled"], false);
    assert_eq!(body["linked"], false);
    assert_eq!(body["linked_rooms"], serde_json::json!([]));
    assert!(body["pending_pairing_code"].is_null());
}
