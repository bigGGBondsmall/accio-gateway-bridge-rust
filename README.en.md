# Accio Gateway Bridge — Rust reconstruction

Independent Rust gateway implementation of the user's Accio bridge contracts, with a new Tauri desktop shell in `desktop/` and UI in `ui/`. The existing tested Node project remains untouched in the sibling `bridge/` directory. This is **not** a compiled or verified Windows release, and no real Accio account/model request has been made.
> **The desktop entry point in use is the Go shell, not the Tauri shell in this package.** `desktop/` here holds Tauri 2 sources that are **not enabled**; they were built once in an isolated Linux cloud workspace (see `desktop/VALIDATION.md`) but have **never been built or launched on Windows** — Tauri requires the MSVC toolchain and this machine only has the gnu target. The shell actually in use is `target\release\accio-desktop.exe` (Go: 7 views, system tray, single instance, `--selftest`), whose sources live in the Node repository's `desktop/`. Either shell is only a shell: the engine is always this package's `accio-gateway-bridge-rust.exe`.

## Run the core

```sh
cargo build --locked --release
cp config.example.json config.json
./target/release/accio-gateway-bridge-rust --config config.json
# --port 0 selects an ephemeral port and prints one JSON readiness record.
```

No configuration file is required to start: missing files use safe empty defaults. Edits create the file; its parent directory must already exist. `ACCIO_BRIDGE_CONFIG` also selects a config file. `--help` describes command options. Existing Node configuration fields/models are accepted; unknown fields survive saves, although not all are active in the Rust implementation.

`ACCIO_BRIDGE_TOKEN` is an **ephemeral data-plane token** for the desktop parent: it protects `/api/bridge/*` and the health check and is never persisted. It is separate from saved `server.token` + `server.enforceToken`, which controls data-plane authentication. Default inference/catalog requests are loopback-local without a token. Never expose a bind publicly without deliberate authentication configuration.

`server.adminToken` is an **optional second secret**. While it is empty or absent the model is exactly the single-token one this gateway always had - one token, every capability - so an existing `config.json`, the running desktop flow and the embedded panel behave unchanged. Once it is configured the capabilities split: inference (generation, official passthrough, readiness), catalogue (the official model list), management (configuration and the status/log/usage reads), discovery (local probes and the model-relay endpoint) and launch (starting a local Accio executable). The data-plane token then keeps inference + catalogue only; management/discovery/launch need `server.adminToken`, sent as `x-gateway-token` or `Authorization: Bearer`, and those scopes are authenticated even when `server.enforceToken` is false, because configuring a second secret is the declaration that they are privileged. A missing or invalid credential answers **401**; a genuine credential that lacks the scope answers **403** (the panel words its prompt on that difference). Neither secret is ever serialised into a response body - `GET /api/bridge/config` only reports the boolean `server.hasAdminToken` - and the runtime `ACCIO_BRIDGE_TOKEN` stays a data-plane credential that does not silently gain the launch scope.

## Implemented gateway features

| Area | Status |
|---|---|
| Accio catalogue `/api/llm/config/v2` | Custom entries + official merge, bounded identity/config/body-keyed single-slot cache and request coalescing |
| Accio generation aliases | OpenAI-compatible/Azure-header/Ollama routing, case-insensitive names resolved in tiers (formal `id` > `modelCode` > alias > `upstreamModel`) with a warning naming both models when a name is ambiguous; a registered-but-disabled model returns 404, unknown names still proxy to the official upstream |
| Message conversion | Text, image parts, system instructions, schema aliases and stable tool-call IDs; tool-call frames now carry the upstream `usageMetadata` like a text-only turn; non-image attachments become explicit metadata notes (not decoded/fetched) |
| Real streaming | SSE event decoding incl. multiline, CR/CRLF, split UTF-8 and EOF; Ollama NDJSON tail; disconnect cancels upstream |
| Stream errors | Malformed, upstream-error and premature EOF never become successful STOP; a length/token-limit or content-filter stop is reported as an error naming that reason (usage preserved) instead of a half-written `finishReason: "TOOL_CALLS"`, while complete-but-malformed argument text is still handed to the client verbatim; no automatic retries/fallback after sending custom request |
| Management | Redacted status, model CRUD (`?id=`, path-form `DELETE /api/bridge/models/<id>`, `POST /api/bridge/models/delete` with body/query), provider groups, model default/flags PATCH, allowlisted config editing |
| Diagnostics | Node-shaped bounded log ring (each line `ts`/`level`/`msg` plus `model`, 1000 entries) with level/count filtering; **selectable recording level** (`debug`/`info`/`warn`/`error`, default `info`) via `ACCIO_BRIDGE_LOG_LEVEL` or `logging.level` (also the legacy `server.logLevel`) at startup, and `GET`/`POST /api/bridge/logging` at runtime — lines below the threshold are never stored, and `debug` lines carry the resolved upstream, its status, the tool count, the tool results the client feeds back and the passthrough target; per-model requests/errors/latency; one log line per distinct forwarded tool set; opt-in `ACCIO_BRIDGE_CAPTURE_DIR` request capture; canceled streams count as errors |
| Management panel | Served by the gateway itself on `/`, `/index.html`, `/dashboard`, `/dashboard/` (reused panel, `no-store`); model actions travel in `data-model-action`/`data-model-toggle` attributes handled by delegated listeners instead of inline `onclick`/`onchange`, so an apostrophe in an id no longer breaks the button and a crafted id cannot run on click; every management call goes through a wrapper that attaches the in-memory `x-gateway-token` (the field accepts `server.token` or `server.adminToken`, the gateway takes either) and a token bar appears on 401 while a 403 says the token lacks the management scope and the admin token is needed, so management works both with and without a second secret; the panel page itself needs no token even when `server.enforceToken` is true (otherwise the page asking for the token would block itself) while management routes stay gated; a stored key is only reported as `hasKey`, so an untouched key field is omitted on save - leaving it blank keeps the key, typing replaces it, clearing removes it |
| Persistence | Serialized clone/validate/write/commit, temporary file + atomic rename; a save is rejected when a `modelCode` or alias equals another model's formal `id` (both models named in the error; a shared `upstreamModel` is still allowed); Unix mode 0600; secrets never in responses |
| Request safety | Host/Origin guards, a per-route capability model (inference/catalogue/management/discovery/launch) satisfied by one token or by a data-plane token plus `server.adminToken`, input/decompression/frame/tool/JSON limits; redirects disabled |
| Official passthrough | Method/status/raw body/query preserved, hop-by-hop headers removed; inbound `cookie`/`authorization`/`x-accio-token` are relayed on every path (Node parity) except this gateway's own `x-gateway-token` |

Discovery and platform Accio inspection/explicit launch are implemented with bounded local probes and metadata-only fixture tests; real native launch is not validated. Native system-tray/startup/Windows installation behavior must be tested on Windows before release.

## Credential boundary

Custom providers receive only their stored provider credential, never incoming browser cookies, official authorization or gateway tokens. The model-relay endpoint (`POST /api/bridge/discover/models`) probes loopback hosts only, plus whatever the owner lists in `server.discoveryAllowedHosts`: its `baseUrl` comes from the request body while the probe can carry a stored apiKey, so an unrestricted relay would aim this gateway's credential at an arbitrary host. A refused host answers 403 before any connection is opened, and the message names the host alone - never the URL and never the key. Official inbound headers are relayed the way the Node proxy does it: everything except hop-by-hop headers, the recomputed `content-length` and this gateway's own `x-gateway-token` — that includes `cookie`, `authorization` and `x-accio-token`, which is how Accio replays its own session on paths such as `POST /api/mcp/ali1688/sgcookie`. A bearer equal to the saved gateway token, the admin token or the runtime token is never relayed. Credential query parameters `token`/`sg_k` are stripped. Response cookies are not passed through. Redirects are never followed. The management panel page itself is served without a token even when `server.enforceToken` is true — otherwise the page asking for the token would block itself — while every management route stays gated; the panel token lives in memory only and is never written to browser storage, cookies, config or the URL.

Editing a destination while retaining a hidden key is rejected: explicitly replace or clear the key. Config files still contain provider secrets in plaintext at rest; OS permissions are not an encrypted keychain. Runtime management tokens do not belong in saved configs.

## Tests (no real API calls)

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --locked
node tests/parity.cjs
```

Protocol fixtures were generated from the independently tested Node baseline using synthetic requests. Black-box tests start only ephemeral loopback mock servers. These tests are not proof of real account compatibility, Windows desktop operation or a performance improvement over Node.

## Reference policy

Design review covered `TF49/AI-Helper`'s MIT-licensed Accio module; no reference runtime was executed. We retained a focused gateway rather than importing its unrelated vendor/account manager, and did not adopt its buffering-before-streaming behavior or tool-ID mismatch. Rust is a deployment/maintainability choice, not evidence of better speed or correctness. The earlier auth reference without a clear root license informed design only; no implementation was copied from it.

## Accio Work 0.37.2 protocol update

See `docs/adk-0372-comparison.md` for the comparison against upstream commit f39fef4, compatibility fixes, intentional differences, and current validation.
