# Deploying Launchpad headless (Docker)

Runs the app without the Tauri shell as two containers, reachable from other
devices on the LAN (e.g. OrbStack on a Mac Studio):

| Service    | Container port | Published port | What it is                                   |
|------------|----------------|----------------|----------------------------------------------|
| `backend`  | 3101           | 3101           | `ao-server` daemon + `ao-engine-tools-cli`    |
| `frontend` | 80             | 8080           | nginx serving the built Vite/React frontend   |

The backend is a plain axum HTTP daemon (`crates/ao-server/src/main.rs`) — it
has **no static file serving** (no `ServeDir` in its router), which is the one
reason the frontend is a second container instead of being served by the
backend itself. The frontend talks to the backend cross-origin; the backend
answers with `CorsLayer::permissive()` (`crates/ao-server/src/routes/mod.rs`),
so no proxying or CORS config is needed.

## Build and run

From the repo root:

```sh
# The URL LAN browsers will use to reach the backend. BAKED INTO THE FRONTEND
# BUNDLE AT BUILD TIME (import.meta.env.VITE_API_BASE_URL, read in
# frontend/src/lib/api.ts) — changing it later requires rebuilding the
# frontend image.
export VITE_API_BASE_URL=http://192.168.1.150:3101

docker compose -f deploy/docker-compose.yml build
docker compose -f deploy/docker-compose.yml up -d
```

Then browse to `http://192.168.1.150:8080` from any LAN device. The backend
health endpoint is `http://192.168.1.150:3101/health` (also used by the
container HEALTHCHECK).

Images are arch-neutral; on Apple Silicon OrbStack they build as aarch64
automatically. The Rust stage uses `rust:1.99-bookworm` to match the
`channel = "1.99"` pin in `rust-toolchain.toml`.

## Environment variables

Baked as image defaults in `deploy/Dockerfile`; override under
`services.backend.environment` in the compose file only to change them.

| Variable | Default | Meaning |
|----------|---------|---------|
| `AO_PORT` | `3101` | Backend listen port (server's own default is 3001). Keep in sync with the port mapping. |
| `AO_BIND_HOST` | `0.0.0.0` | Bind address. The server defaults to `127.0.0.1`; LAN exposure requires `0.0.0.0`. **Security note:** the webhook `INSECURE_NO_AUTH` bypass only applies on a loopback bind (`crates/ao-server/src/webhook_gateway.rs`), so on `0.0.0.0` every webhook route requires a real HMAC secret — there is no unauthenticated webhook path in this deployment. |
| `LAUNCHPAD_STUDIO_DATA_DIR` | `/data` | Data root override (`ao_protocol::data_root`). Everything persistent lives here — agents, transcripts, memories, scheduled tasks, `providers.toml`, the secret vault, the workspace lock — and `/data` is the named volume `launchpad-data`. |
| `LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK` | `1` | Forces the file-backed secret vault (`<data-root>/secret_vault.json`, mode 0600). A container has no OS keychain; without this the vault probes (and on headless Linux, fails to reach) a Secret Service daemon. Aliases `LAUNCHPAD_MCP_STORE_FILE_FALLBACK`, `LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK`, `LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK` work too; `LAUNCHPAD_STUDIO_NO_KEYCHAIN=1` is the equivalent kill switch. |
| `LAUNCHPAD_MCP_SESSION_TTL_SECS` | `3600` | Optional. MCP session eviction TTL (`ao-server` startup). |
| `RUST_LOG` | (crate defaults) | Optional tracing filter override. |

`VITE_API_BASE_URL` is a **build argument**, not a runtime variable — see
above. No other `VITE_*` variable is required; `VITE_BUILD_DATE` is stamped
automatically by `frontend/vite.config.ts`.

## Secrets after deploy

Nothing secret is in the images or the compose file. Configure per-provider
and per-channel secrets after the stack is up:

### Model provider API keys (Anthropic / OpenAI / OpenRouter / Gemini)

Normal path: paste the key in the web UI's agent editor — the backend vaults
it (into `/data/secret_vault.json` here, since the file fallback is forced).

Headless path: write `providers.toml` into the data volume, modeled on the
repo's `providers.toml.example`:

```sh
docker compose -f deploy/docker-compose.yml cp providers.toml backend:/data/providers.toml
docker compose -f deploy/docker-compose.yml restart backend
```

On first read each plaintext `api_key` is copied into the file-backed vault
(and left in the file when no keychain is reachable, so the file keeps
working as a provisioning channel — see `absorb_plaintext_api_keys` in
`crates/ao-engine-tools-provider-config/src/lib.rs`).

### Channel tokens (Telegram / Matrix / Discord / Slack / Email)

Use the web UI: agent settings expose per-channel token/secret fields
(`PUT /agents/{id}/telegram/token`, `PUT /agents/{id}/matrix/connection`,
`PUT /agents/{id}/channels/{kind}/secret`, ...). All are write-only and land
in the same file-backed vault on the volume, so they survive container
recreation. Alternatively, any channel secret can be *read* from a
`LAUNCHPAD_CHANNEL_SECRET__<AGENT_ID>__<BINDING_ID>__<ROLE>` environment
variable (read-only backend that outranks the vault — see
`crates/ao-engine-tools-provider-config/src/channel_secret_store.rs`), but
writes always go to the vault file.

## The CLI

`ao-engine-tools-cli` (the stdin/stdout dogfood REPL for the Anthropic
provider) is installed in the backend image alongside the server:

```sh
docker compose -f deploy/docker-compose.yml exec backend \
  ao-engine-tools-cli --provider anthropic
```

It reads the same data root (`/data/providers.toml`) as the server.

## Data and backups

All persistent state is in the `launchpad-data` named volume mounted at
`/data`. To back up or inspect it:

```sh
docker run --rm -v launchpad-studio_launchpad-data:/data -v "$PWD":/backup \
  alpine tar czf /backup/launchpad-data.tgz -C /data .
```

(The volume's compose-prefixed name is `launchpad-studio_launchpad-data` when
run from the repo root; confirm with `docker volume ls`.)

## Known limitations of the headless build

- Parts of the frontend import `@tauri-apps/api` (dialogs, notifications,
  window management). In a plain browser those APIs are absent; the core
  chat/settings flows go over plain HTTP/SSE and work, but expect individual
  Tauri-only affordances to no-op or degrade.
- The backend binds all interfaces with no authentication of its own — treat
  port 3101 as trusted-LAN-only, or put it behind an authenticating reverse
  proxy before exposing it further.
