mod accio;
mod config;
mod discovery;
mod management;
mod protocol;
mod stream;
mod telemetry;
use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::RwLock;
type CatalogCache = Arc<tokio::sync::Mutex<Option<(String, std::time::Instant, Vec<Value>)>>>;
#[derive(Clone)]
struct App {
    config: Arc<RwLock<Value>>,
    path: Arc<PathBuf>,
    /// The official origin and every remote provider: honours the environment proxy
    /// (`HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`), which is what a user behind a corporate proxy
    /// needs.
    client: reqwest::Client,
    /// Loopback endpoints. One network policy, two clients: a request to 127.0.0.1 must never be
    /// sent through a proxy configured for the internet, and it used to depend on the caller.
    local_client: reqwest::Client,
    runtime_token: Option<String>,
    telemetry: telemetry::Shared,
    catalog_cache: CatalogCache,
    /// Signature of the tool set reported last, so a distinct set is logged once instead
    /// of on every turn.
    last_tools: Arc<std::sync::Mutex<Option<String>>>,
}
fn error(status: u16, msg: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({"ok":false,"error":msg})),
    )
        .into_response()
}
fn json_response(v: Value) -> Response {
    Json(v).into_response()
}
/// The one shape every routing answer uses: the 404 for a path this gateway does not serve and the
/// 405 for a method a path does not accept. It is the body the official upstream returns for a path
/// it does not know (`timestamp` / `path` / `status` / `error` / `requestId`), so a caller sees the
/// same error shape whether the miss is answered here or by the origin. `requestId` is generated
/// locally: no upstream request is involved in a local routing miss.
fn standard_error(status: u16, label: &str, path: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({
            "timestamp": crate::discovery::timestamp(),
            "path": path,
            "status": status,
            "error": label,
            "requestId": local_request_id(),
        })),
    )
        .into_response()
}
/// Correlation id for a locally answered routing miss, in the same shape as the one the official
/// upstream puts in its error body.
fn local_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) % 1_000_000;
    format!("{nanos:08x}-{sequence:06}")
}
fn not_found(path: &str) -> Response {
    standard_error(404, "Not Found", path)
}
fn method_not_allowed(path: &str) -> Response {
    standard_error(405, "Method Not Allowed", path)
}

fn header<'a>(h: &'a HeaderMap, key: &str) -> &'a str {
    h.get(key).and_then(|v| v.to_str().ok()).unwrap_or("")
}
fn canonical(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path == "/__bridge/accio" || path == "/__bridge/accio/launch" {
        return path.replacen("/__bridge/", "/api/bridge/", 1);
    }
    if path == "/__bridge/status" {
        return "/api/bridge/status".into();
    }
    if let Some(s) = path.strip_prefix("/__bridge/api/") {
        return format!("/api/bridge/{s}");
    }
    path.trim_end_matches('/').to_string()
}
/// What a route needs from its caller. The gateway used to accept one token for all of this at
/// once, so the credential the desktop parent injects for plain inference could also rewrite the
/// configuration, read the log ring (prompts and tool names) and start a local executable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    /// Data plane: generation, the official passthrough and readiness.
    Inference,
    /// The official catalogue the Accio client fetches before a turn.
    Catalogue,
    /// Configuration edits and every read derived from the configuration.
    Management,
    /// Local host probes and the model-relay endpoint.
    Discovery,
    /// Inspecting and starting a local Accio executable.
    Launch,
}
impl Scope {
    fn label(self) -> &'static str {
        match self {
            Scope::Inference => "inference",
            Scope::Catalogue => "catalogue",
            Scope::Management => "management",
            Scope::Discovery => "discovery",
            Scope::Launch => "launch",
        }
    }
    /// Capabilities the data-plane credential keeps once a second secret exists: the Accio client
    /// and the desktop parent drive these without ever holding the admin secret.
    fn data_plane(self) -> bool {
        matches!(self, Scope::Inference | Scope::Catalogue)
    }
}
/// Route -> capability table, mirroring the dispatch order of `dispatch`; keep the two in step.
///
/// - Inference: `GET /healthz` and `/__bridge/health`; `POST /api/adk/llm`,
///   `/api/adk/llm/generateContent`, `/api/adk/llm/generateContent/stream` and `/api/bridge/test`;
///   and every path that is not a gateway route of its own (the transparent passthrough).
/// - Catalogue: `POST /api/llm/config/v2`.
/// - Management: `GET /api/bridge/status|config|models|usage|logs|logging|providers`,
///   `PUT /api/bridge/config`, `POST|DELETE /api/bridge/models`, `POST /api/bridge/models/delete`,
///   `PATCH|DELETE /api/bridge/models/*`, `POST|PATCH|DELETE /api/bridge/providers*`, and any other
///   `/api/bridge/*` or `/__bridge/*` path this table has not caught up with (answered 404).
/// - Discovery: `GET /api/bridge/discover`, `POST /api/bridge/discover/models` and
///   `GET /api/bridge/accio` (it runs tasklist/pgrep and honours `?exePath=`).
/// - Launch: `POST /api/bridge/accio/launch`.
fn required_scope(method: &str, path: &str) -> Scope {
    if path == "/api/bridge/config"
        || path == "/api/bridge/status"
        || path == "/api/bridge/usage"
        || path == "/api/bridge/logs"
        || path == "/api/bridge/logging"
        || (path == "/api/bridge/models" && method == "GET")
        || (path == "/api/bridge/providers" && method == "GET")
    {
        return Scope::Management;
    }
    if (path.starts_with("/api/bridge/providers") && ["POST", "PATCH", "DELETE"].contains(&method))
        || (path == "/api/bridge/models" && ["POST", "DELETE"].contains(&method))
        || (path == "/api/bridge/models/delete" && method == "POST")
        || (path.starts_with("/api/bridge/models/") && ["PATCH", "DELETE"].contains(&method))
    {
        return Scope::Management;
    }
    if path == "/api/bridge/accio/launch" && method == "POST" {
        return Scope::Launch;
    }
    if path == "/api/bridge/accio" || path.starts_with("/api/bridge/discover") {
        return Scope::Discovery;
    }
    if path == "/api/llm/config/v2" && method == "POST" {
        return Scope::Catalogue;
    }
    if path == "/healthz" || path == "/__bridge/health" || path == "/api/bridge/test" {
        return Scope::Inference;
    }
    if path.starts_with("/api/bridge/") || path.starts_with("/__bridge/") {
        // Anything else under the management prefix is a path whose shape this table has not caught
        // up with: refuse it as management (and `dispatch` then answers 404 for it).
        return Scope::Management;
    }
    Scope::Inference
}
/// Outcome of a credential check. The two rejections are deliberately different statuses: 401 means
/// "no usable credential was presented", 403 means "the credential is genuine but this route needs
/// a capability it does not hold". The panel has to tell them apart, because once an admin token
/// exists, re-entering the same inference token would fail forever.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    Granted,
    Unauthorized,
    Forbidden,
}
/// Capability check for one request.
///
/// `server.token` is the data-plane credential; `server.adminToken` is an optional second secret.
/// While `adminToken` is empty or absent the model is exactly the single-token model this gateway
/// has always had - one token, every scope - so an existing configuration and the running desktop
/// flow behave unchanged. Once an admin token is configured, the data-plane token keeps Inference
/// and Catalogue only, Management/Discovery/Launch need the admin token, and those privileged
/// scopes are authenticated even on loopback with `enforceToken` off: configuring a second secret
/// is the declaration that management is supposed to be privileged. The admin secret is accepted
/// through the same two headers as the gateway token, so one panel field can hold either.
fn authorize(h: &HeaderMap, c: &Value, scope: Scope, remote: bool) -> Access {
    let token = c["server"]["token"].as_str().unwrap_or("");
    let admin = c["server"]["adminToken"].as_str().unwrap_or("");
    let separated = !admin.is_empty();
    let required =
        c["server"]["enforceToken"] == true || remote || (separated && !scope.data_plane());
    if !required {
        return Access::Granted;
    }
    if token.is_empty() && !separated {
        return Access::Unauthorized;
    }
    let candidate = header(h, "x-gateway-token");
    let bearer = bearer_token(header(h, "authorization"));
    if candidate.is_empty() && bearer.is_empty() {
        return Access::Unauthorized;
    }
    if separated && (constant_equal(admin, candidate) || constant_equal(admin, bearer)) {
        return Access::Granted;
    }
    if !token.is_empty() && (constant_equal(token, candidate) || constant_equal(token, bearer)) {
        return if !separated || scope.data_plane() {
            Access::Granted
        } else {
            Access::Forbidden
        };
    }
    Access::Unauthorized
}
fn constant_equal(a: &str, b: &str) -> bool {
    let mut diff = a.len() ^ b.len();
    for (i, x) in a.bytes().enumerate() {
        diff |= (x ^ b.as_bytes().get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}
fn validate_origin(h: &HeaderMap, cfg: &Value) -> bool {
    let host = header(h, "host");
    let Ok(hosturl) = url::Url::parse(&format!("http://{host}")) else {
        return false;
    };
    if !hosturl.username().is_empty()
        || hosturl.password().is_some()
        || hosturl.path() != "/"
        || hosturl.query().is_some()
        || hosturl.fragment().is_some()
    {
        return false;
    }
    let hostname = hosturl.host_str().unwrap_or("");
    let configured = cfg["server"]["host"].as_str().unwrap_or("127.0.0.1");
    let explicit = hostname.trim_matches(['[', ']']) == configured.trim_matches(['[', ']']);
    let wildcard = ["0.0.0.0", "::", "[::]"].contains(&configured)
        && hostname
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok();
    if !config::is_loopback(hostname) && !explicit && !wildcard {
        return false;
    }

    if let Some(origin) = h.get("origin") {
        let Ok(origin) = origin.to_str() else {
            return false;
        };
        let Ok(url) = url::Url::parse(origin) else {
            return false;
        };
        return ["http", "https", "tauri"].contains(&url.scheme())
            && config::is_loopback(url.host_str().unwrap_or(""));
    }
    true
}
fn decode_bytes(bytes: &[u8], encoding: &str, max: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let encoding = if encoding.is_empty() && bytes.starts_with(&[0x1f, 0x8b]) {
        "gzip"
    } else {
        encoding
    };
    // HTTP deflate is normally zlib-wrapped, but some gateways send raw DEFLATE.
    if encoding == "deflate" {
        let mut decoded = vec![];
        let result = flate2::read::ZlibDecoder::new(bytes)
            .take(max as u64 + 1)
            .read_to_end(&mut decoded);
        if result.is_err() {
            decoded.clear();
            flate2::read::DeflateDecoder::new(bytes)
                .take(max as u64 + 1)
                .read_to_end(&mut decoded)
                .map_err(|_| "invalid compressed body")?;
        }
        if decoded.len() > max {
            return Err("decoded body too large".into());
        }
        return Ok(decoded);
    }
    let reader: Box<dyn Read> = match encoding {
        "" | "identity" => Box::new(bytes),
        "gzip" => Box::new(flate2::read::GzDecoder::new(bytes)),
        "br" => Box::new(brotli::Decompressor::new(bytes, 4096)),
        _ => return Err("unsupported content encoding".into()),
    };
    let mut decoded = vec![];
    reader
        .take(max as u64 + 1)
        .read_to_end(&mut decoded)
        .map_err(|_| "invalid compressed body")?;
    if decoded.len() > max {
        return Err("decoded body too large".into());
    }
    Ok(decoded)
}
fn decode(bytes: &[u8], encoding: &str, max: usize) -> Result<Value, String> {
    let decoded = decode_bytes(bytes, encoding, max)?;
    serde_json::from_slice(
        decoded
            .strip_prefix(&[0xef, 0xbb, 0xbf])
            .unwrap_or(&decoded),
    )
    .map_err(|_| "invalid JSON".into())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Accio Gateway Bridge Rust {}\nUsage: accio-gateway-bridge-rust [--config PATH] [--host IP] [--port PORT]\nACCIO_BRIDGE_CONFIG: config path; ACCIO_BRIDGE_TOKEN: ephemeral data-plane token (management only while server.adminToken is unset).\nDefaults to loopback:8080. Port 0 selects a free port and prints JSON readiness.\nA missing config starts with safe empty defaults; model/config edits create it.\n--version prints version. No credential discovery or model calls at startup.",env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut index = 1;
    while index < args.len() {
        if !["--config", "--host", "--port"].contains(&args[index].as_str())
            || index + 1 >= args.len()
        {
            return Err(std::io::Error::other("invalid CLI arguments; use --help").into());
        }
        index += 2;
    }

    let arg = |key: &str| {
        args.iter()
            .position(|s| s == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let path = PathBuf::from(
        arg("--config")
            .or_else(|| std::env::var("ACCIO_BRIDGE_CONFIG").ok())
            .unwrap_or_else(|| "config.json".into()),
    );
    let raw = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => config::defaults(),
        Err(e) => return Err(e.into()),
    };
    let mut c = config::normalize(raw).map_err(std::io::Error::other)?;
    if let Some(host) = arg("--host") {
        c["server"]["host"] = json!(host);
    }
    if let Some(port) = arg("--port") {
        c["server"]["port"] = json!(port.parse::<u16>()?);
    }
    let runtime_token = std::env::var("ACCIO_BRIDGE_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    c = config::normalize(c).map_err(std::io::Error::other)?;
    // A configuration whose model names collide is refused on save, but a file that already has the
    // collision must still start: warn (on stderr, before the log ring exists) and carry on, so the
    // owner can repair it in the panel instead of being locked out.
    if let Some(conflict) = config::route_name_conflict(&c) {
        eprintln!("config: {conflict}");
    }
    let address = format!(
        "{}:{}",
        c["server"]["host"].as_str().unwrap(),
        c["server"]["port"].as_u64().unwrap_or(8080)
    );
    // One place decides the proxy policy: `client` keeps the environment proxy, `local_client`
    // never uses it because everything it talks to is on this machine.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    let local_client = discovery::client().map_err(std::io::Error::other)?;
    // Recording level: env wins over config, and POST /api/bridge/logging changes it at runtime.
    let initial_log_level = std::env::var("ACCIO_BRIDGE_LOG_LEVEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| c["logging"]["level"].as_str().map(str::to_string))
        .or_else(|| c["server"]["logLevel"].as_str().map(str::to_string));
    let app = App {
        config: Arc::new(RwLock::new(c)),
        path: Arc::new(path),
        client,
        local_client,
        runtime_token,
        telemetry: Default::default(),
        catalog_cache: Default::default(),
        last_tools: Default::default(),
    };
    if let Some(level) = initial_log_level {
        if let Err(reason) = telemetry::set_level(&app.telemetry, &level) {
            eprintln!("logging: {reason}");
        }
    }
    let listener = tokio::net::TcpListener::bind(&address).await?;
    let local = listener.local_addr()?;
    app.config.write().await["server"]["port"] = json!(local.port());
    println!(
        "{}",
        json!({"event":"listening","host":local.ip().to_string(),"port":local.port()})
    );
    let router = Router::new().fallback(any(handle)).with_state(app);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
async fn handle(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let mut cfg = app.config.read().await.clone();
    let path = canonical(req.uri().path());
    if let Some(token) = &app.runtime_token {
        if path.starts_with("/api/bridge/") || path.starts_with("/__bridge/") || path == "/healthz"
        {
            cfg["server"]["token"] = json!(token);
            cfg["server"]["enforceToken"] = json!(true);
        }
    }
    // The desktop parent's ephemeral token becomes this process' data-plane credential: it covers
    // /api/bridge/* and the health check. Once `server.adminToken` is configured it no longer
    // reaches the management, discovery or launch scopes, so a shell that needs those has to be
    // given the admin secret instead of assuming the token it injected is enough.
    let scope = required_scope(req.method().as_str(), &path);
    let method = req.method().clone();
    let panel = is_panel_path(method.as_str(), &path);
    let headers = req.headers().clone();
    let uri = req.uri().clone();
    if !validate_origin(&headers, &cfg) {
        return error(403, "untrusted Host or browser Origin");
    }
    if method == "OPTIONS" {
        let mut r = StatusCode::NO_CONTENT.into_response();
        if cfg["server"]["cors"] != false {
            if let Some(origin) = headers.get("origin") {
                r.headers_mut()
                    .insert("access-control-allow-origin", origin.clone());
            }
            r.headers_mut().insert(
                "access-control-allow-headers",
                "content-type,x-gateway-token,authorization"
                    .parse()
                    .unwrap(),
            );
            r.headers_mut().insert(
                "access-control-allow-methods",
                "GET,POST,PUT,PATCH,DELETE,OPTIONS".parse().unwrap(),
            );
            return r;
        }
    }
    // The panel itself is static HTML with no secrets; it stays reachable so the page can ask for
    // the token. Every other route needs the capability from the table above. The check happens
    // here rather than inside `dispatch`, so a rejected request never reads its body first.
    if !panel {
        match authorize(
            &headers,
            &cfg,
            scope,
            !peer.ip().is_loopback()
                || !config::is_loopback(cfg["server"]["host"].as_str().unwrap_or("")),
        ) {
            Access::Granted => {}
            Access::Unauthorized => return error(401, "invalid gateway token"),
            Access::Forbidden => {
                return error(
                    403,
                    &format!(
                        "gateway token accepted but it lacks the {} scope; send the admin token (server.adminToken) for this route",
                        scope.label()
                    ),
                )
            }
        }
    }
    let max = cfg["server"]["maxBodyBytes"].as_u64().unwrap_or(26214400) as usize;
    let bytes = match to_bytes(req.into_body(), max).await {
        Ok(b) => b,
        Err(_) => return error(413, "request body too large"),
    };
    let mut response = dispatch(&app, &cfg, &path, &method, &headers, &uri, &bytes, max).await;
    if cfg["server"]["cors"] != false {
        if let Some(origin) = headers.get("origin") {
            response
                .headers_mut()
                .insert("access-control-allow-origin", origin.clone());
            response
                .headers_mut()
                .insert("vary", "Origin".parse().unwrap());
        }
    }
    response
}
/// How a requested model name resolves against the configured models.
enum RouteOutcome<'a> {
    Found(&'a Value, Option<String>),
    Disabled(String),
    Unknown,
}

/// Resolve `requested` to a configured model.
///
/// Matching happens in tiers: a formal `id` always wins over a `modelCode`, which wins over an
/// alias, which wins over an `upstreamModel`. The previous first-match-wins scan over every name
/// at once let an earlier model's alias (or upstream name) shadow a later model's real id, so a
/// request could be forwarded to a different provider than the one the caller named.
fn route_model<'a>(models: &'a [Value], requested: &str) -> RouteOutcome<'a> {
    if requested.trim().is_empty() {
        return RouteOutcome::Unknown;
    }
    let same = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case(requested))
    };
    let matches_name = |m: &Value| {
        same(m.get("id"))
            || same(m.get("modelCode"))
            || same(m.get("upstreamModel"))
            || m["aliases"]
                .as_array()
                .is_some_and(|a| a.iter().any(|s| same(Some(s))))
    };
    for tier in ["id", "modelCode", "aliases", "upstreamModel"] {
        let hits: Vec<&Value> = models
            .iter()
            .filter(|m| match tier {
                "id" => same(m.get("id")),
                "modelCode" => same(m.get("modelCode")),
                "aliases" => m["aliases"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|s| same(Some(s)))),
                _ => same(m.get("upstreamModel")),
            })
            .collect();
        if hits.is_empty() {
            continue;
        }
        let enabled: Vec<&Value> = hits
            .iter()
            .copied()
            .filter(|m| m["enabled"] != false)
            .collect();
        if enabled.is_empty() {
            // The name exists but is switched off. Report that instead of letting a lower tier
            // answer for it: another enabled model's alias must not take over a formal id, and a
            // silent reroute is exactly the confusion this resolution order exists to remove.
            return RouteOutcome::Disabled(hits[0]["id"].as_str().unwrap_or(requested).to_string());
        }
        // Say so when another enabled model also answers to this name, so an ambiguous
        // configuration is visible in the log instead of only showing up as a wrong provider.
        let winner = enabled[0]["id"].as_str().unwrap_or("?");
        let others: Vec<String> = models
            .iter()
            .filter(|m| m["enabled"] != false)
            .filter(|m| m["id"].as_str() != enabled[0]["id"].as_str())
            .filter(|m| matches_name(m))
            .map(|m| m["id"].as_str().unwrap_or("?").to_string())
            .collect();
        let note = (!others.is_empty()).then(|| {
            format!(
                "model name {requested} also matches {}; routing to {winner} by {tier}",
                others.join(", ")
            )
        });
        return RouteOutcome::Found(enabled[0], note);
    }
    RouteOutcome::Unknown
}
#[allow(clippy::too_many_arguments)] // Explicit request metadata avoids retaining a consumed HTTP body.
async fn dispatch(
    app: &App,
    cfg: &Value,
    path: &str,
    method: &axum::http::Method,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    bytes: &[u8],
    max: usize,
) -> Response {
    let models = cfg["models"].as_array().unwrap();
    let public: Vec<Value> = models.iter().map(config::public_model).collect();
    // The management panel is served by the gateway itself, on the same paths the older build
    // used, so the URL people already had keeps working. `canonical()` reduces a trailing slash
    // to nothing, which makes "/" an empty string and "/dashboard/" a plain "/dashboard".
    // The page is GET-only, and every one of those paths says so the same way: a method the path
    // does not accept is a 405, not a different answer per path (the old build proxied some of
    // them, so `POST /` came back 405 from the origin while `POST /dashboard` came back 404).
    if is_panel_route(path) {
        if method == "GET" {
            return panel_response();
        }
        return method_not_allowed(uri.path());
    }
    if method == "GET" && (path == "/healthz" || path == "/__bridge/health") {
        return json_response(
            json!({"ok":true,"upstream":cfg["upstream"]["baseUrl"],"models":models.iter().filter(|m|m["enabled"]!=false).map(|m|m["id"].clone()).collect::<Vec<_>>()}),
        );
    }
    if method == "GET" && path == "/api/bridge/status" {
        return json_response(
            json!({"ok":true,"status":"running","version":env!("CARGO_PKG_VERSION"),"server":{"host":cfg["server"]["host"],"port":cfg["server"]["port"]},"upstream":cfg["upstream"]["baseUrl"],"modelsCount":models.len(),"activeModelsCount":models.iter().filter(|m|m["enabled"]!=false).count(),"models":public}),
        );
    }
    if path == "/api/bridge/providers" && method == "GET" {
        return json_response(json!({"ok":true,"providers":management::providers(cfg)}));
    }
    if (path.starts_with("/api/bridge/providers")
        && (method == "POST" || method == "PATCH" || method == "DELETE"))
        || (path.starts_with("/api/bridge/models/")
            && path != "/api/bridge/models/delete"
            && (method == "PATCH" || method == "DELETE"))
    {
        let body = if bytes.is_empty() {
            json!({})
        } else {
            match decode(bytes, header(headers, "content-encoding"), max) {
                Ok(v) => v,
                Err(e) => return error(400, &e),
            }
        };
        let mut guard = app.config.write().await;
        let mut next = guard.clone();
        let result = match management::mutate(&mut next, method.as_str(), path, &body) {
            Ok(v) => v,
            Err((status, message)) => return error(status, &message),
        };
        // Two models answering to one name is the caller's mistake, so report it as such rather
        // than as a gateway fault. `config::save` re-checks as the last gate before disk.
        if let Some(conflict) = config::route_name_conflict(&next) {
            return error(400, &conflict);
        }
        // A save that `disableDiskSave` skipped is reported and not counted as an edit that reached
        // the disk: a caller may not be told a configuration was saved when it was only kept in
        // memory. The flag's purpose is unchanged — nothing is written.
        let persisted = match config::save(&app.path, &next).await {
            Ok(persisted) => persisted,
            Err(e) => return error(500, &e),
        };
        *guard = next;
        let mut out = result;
        out["persisted"] = json!(persisted);
        return json_response(out);
    }
    if path == "/api/bridge/config" && (method == "GET" || method == "PUT") {
        if method == "GET" {
            return json_response(json!({"ok":true,"config":safe_config(cfg)}));
        }
        let input = match decode(bytes, header(headers, "content-encoding"), max) {
            Ok(v) => v,
            Err(e) => return error(400, &e),
        };
        if !input.is_object() {
            return error(400, "configuration patch must be an object");
        }
        let mut guard = app.config.write().await;
        let mut next = guard.clone();
        for (section, keys) in [
            ("server", vec!["maxBodyBytes", "cors"]),
            ("upstream", vec!["baseUrl", "timeoutMs"]),
            (
                "catalog",
                vec![
                    "enabled",
                    "fetchOfficial",
                    "providerName",
                    "providerDisplayName",
                    "timeoutMs",
                ],
            ),
        ] {
            if let Some(patch) = input.get(section) {
                let Some(patch) = patch.as_object() else {
                    return error(400, "configuration section must be an object");
                };
                for (k, v) in patch {
                    if !keys.contains(&k.as_str()) {
                        return error(400, "unsupported configuration property");
                    }
                    let old = &next[section][k];
                    if (old.is_boolean() && !v.is_boolean())
                        || (old.is_string() && !v.is_string())
                        || (old.is_number() && (!v.is_u64() || v.as_u64() == Some(0)))
                    {
                        return error(400, "invalid configuration property type or range");
                    }
                    if section == "upstream"
                        && k == "baseUrl"
                        && v != old
                        && !next["upstream"]["apiKey"].as_str().unwrap_or("").is_empty()
                    {
                        return error(400,"cannot change upstream destination while a credential is saved; edit config explicitly");
                    }
                    next[section][k] = v.clone();
                }
            }
        }
        if next["server"]["maxBodyBytes"].as_u64().unwrap_or(0) > 100 * 1024 * 1024 {
            return error(400, "body limit must not exceed 100 MiB");
        }
        let next = match config::normalize(next) {
            Ok(v) => v,
            Err(e) => return error(400, &e),
        };
        let persisted = match config::save(&app.path, &next).await {
            Ok(persisted) => persisted,
            Err(e) => return error(500, &e),
        };
        *guard = next;
        return json_response(
            json!({"ok":true,"persisted":persisted,"config":safe_config(&guard)}),
        );
    }
    if method == "GET" && path == "/api/bridge/models" {
        return json_response(json!({"ok":true,"models":public}));
    }
    let post_delete = path == "/api/bridge/models/delete" && method == "POST";
    if (path == "/api/bridge/models" && (method == "POST" || method == "DELETE")) || post_delete {
        let mut guard = app.config.write().await;
        let mut next = guard.clone();
        let list = next["models"].as_array_mut().unwrap();
        let result = if method == "DELETE" || post_delete {
            let body = if post_delete && !bytes.is_empty() {
                match decode(bytes, header(headers, "content-encoding"), max) {
                    Ok(v) => v,
                    Err(e) => return error(400, &e),
                }
            } else {
                json!({})
            };
            if body.get("id").is_some_and(|id| !id.is_string()) {
                return error(400, "id must be a string");
            }
            let query_id = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                .find(|(k, _)| k == "id")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            let id = body["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .unwrap_or(&query_id)
                .to_owned();
            if id.is_empty() {
                return error(400, "id required");
            }
            let len = list.len();
            list.retain(|m| !m["id"].as_str().unwrap_or("").eq_ignore_ascii_case(&id));
            if list.len() == len {
                return error(404, "model not found");
            }
            json!({"deleted":id})
        } else {
            let mut input = match decode(bytes, header(headers, "content-encoding"), max) {
                Ok(v) => v,
                Err(e) => return error(400, &e),
            };
            let id = input["id"].as_str().unwrap_or("").to_string();
            let index = list
                .iter()
                .position(|m| m["id"].as_str().unwrap_or("").eq_ignore_ascii_case(&id));
            if let Some(i) = index {
                let old = &list[i];
                if input.get("apiKey").is_none()
                    && !old["apiKey"].as_str().unwrap_or("").is_empty()
                    && (input.get("baseUrl").is_some_and(|v| v != &old["baseUrl"])
                        || input.get("provider").is_some_and(|v| v != &old["provider"]))
                {
                    return error(400,"Changing provider or base URL requires explicitly replacing or clearing API key");
                }
                let mut merged = old.clone();
                if let Some(obj) = input.as_object() {
                    for (k, v) in obj {
                        merged[k] = v.clone();
                    }
                }
                input = merged;
            }
            let model = match config::normalize_model(input) {
                Ok(m) => m,
                Err(e) => return error(400, &e),
            };
            if model["isDefault"] == true {
                for entry in list.iter_mut() {
                    entry["isDefault"] = json!(false);
                }
            }
            if let Some(i) = index {
                list[i] = model.clone();
            } else {
                list.push(model.clone());
            }
            json!({"model":config::public_model(&model)})
        };
        if let Some(conflict) = config::route_name_conflict(&next) {
            return error(400, &conflict);
        }
        let persisted = match config::save(&app.path, &next).await {
            Ok(persisted) => persisted,
            Err(e) => return error(500, &e),
        };
        *guard = next;
        let mut out = result;
        out["ok"] = json!(true);
        out["success"] = json!(true);
        out["persisted"] = json!(persisted);
        out["models"] = json!(guard["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(config::public_model)
            .collect::<Vec<_>>());
        return json_response(out);
    }
    if method == "POST" && path == "/api/llm/config/v2" {
        return catalog(app, cfg, headers, bytes).await;
    }
    if method == "POST"
        && [
            "/api/adk/llm",
            "/api/adk/llm/generateContent",
            "/api/adk/llm/generateContent/stream",
            "/api/bridge/test",
        ]
        .contains(&path)
    {
        let mut body = match decode(bytes, header(headers, "content-encoding"), max) {
            Ok(v) => v,
            Err(e) => return error(400, &e),
        };
        if path == "/api/bridge/test" && body.get("contents").is_none() {
            let prompt = body["prompt"]
                .as_str()
                .unwrap_or("Say hello briefly.")
                .to_string();
            body["contents"] = json!([{"role":"user","parts":[{"text":prompt}]}]);
        }
        match route_model(models, body["model"].as_str().unwrap_or("")) {
            RouteOutcome::Found(model, note) => {
                if let Some(note) = note {
                    telemetry::log(&app.telemetry, "warn", &note);
                }
                return generate(app, cfg, model, &body).await;
            }
            // A registered model that is switched off is not an unknown upstream model: say so
            // instead of forwarding the request to the official gateway as if nothing matched.
            RouteOutcome::Disabled(id) => {
                telemetry::log(
                    &app.telemetry,
                    "warn",
                    &format!("model {id} is configured but disabled; enable it to use it"),
                );
                return error(
                    404,
                    &format!(
                        "model {id} is configured but disabled; enable it in the gateway panel"
                    ),
                );
            }
            RouteOutcome::Unknown => {
                if path == "/api/bridge/test" {
                    return error(404, "custom model not found");
                }
            }
        }
    }
    if method == "GET" && path == "/api/bridge/usage" {
        return json_response(telemetry::usage(&app.telemetry));
    }
    if method == "GET" && path == "/api/bridge/logs" {
        return json_response(telemetry::logs(&app.telemetry, uri.query().unwrap_or("")));
    }
    if path == "/api/bridge/logging" {
        if method == "GET" {
            return json_response(json!({
                "ok": true,
                "level": telemetry::level(&app.telemetry),
                "levels": telemetry::LEVELS,
                "sources": "ACCIO_BRIDGE_LOG_LEVEL, config logging.level, POST {level}"
            }));
        }
        if method == "POST" {
            let body = match decode(bytes, header(headers, "content-encoding"), max) {
                Ok(value) => value,
                Err(message) => return error(400, &message),
            };
            return match telemetry::set_level(&app.telemetry, body["level"].as_str().unwrap_or(""))
            {
                Ok(level) => {
                    telemetry::log(
                        &app.telemetry,
                        "info",
                        &format!("log recording level set to {level}"),
                    );
                    json_response(json!({"ok": true, "level": level, "levels": telemetry::LEVELS}))
                }
                Err(message) => error(400, &message),
            };
        }
        return error(405, "method not allowed");
    }
    if method == "GET" && path == "/api/bridge/discover" {
        return json_response(discovery::discover(cfg, &app.local_client).await);
    }
    if method == "POST" && path == "/api/bridge/discover/models" {
        let body = match decode(bytes, header(headers, "content-encoding"), max) {
            Ok(v) => v,
            Err(e) => return error(400, &e),
        };
        return match discovery::list_models(cfg, &app.local_client, &body).await {
            Ok(v) => json_response(v),
            Err((status, e)) => error(status, &e),
        };
    }
    if method == "GET" && path == "/api/bridge/accio" {
        let mut inspect_cfg = cfg.clone();
        if let Some((_, path)) = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .find(|(k, _)| k == "exePath")
        {
            if !inspect_cfg["accio"].is_object() {
                inspect_cfg["accio"] = json!({});
            }
            inspect_cfg["accio"]["customExePath"] = json!(path);
        }
        return json_response(
            json!({"ok":true,"accio":accio::inspect(&inspect_cfg).await,"bridgeGatewayUrl":format!("http://{}:{}",cfg["server"]["host"].as_str().unwrap_or("127.0.0.1"),cfg["server"]["port"])}),
        );
    }
    if method == "POST" && path == "/api/bridge/accio/launch" {
        let body = match decode(bytes, header(headers, "content-encoding"), max) {
            Ok(v) => v,
            Err(e) => return error(400, &e),
        };
        return match accio::launch(cfg, &body).await {
            Ok(v) => json_response(v),
            Err((status, e)) => (
                StatusCode::from_u16(status).unwrap(),
                Json(json!({"ok":false,"needsForce":e.starts_with("needsForce:"),"error":e})),
            )
                .into_response(),
        };
    }
    // No route under the management prefix is recognised-but-unimplemented any more, so an unknown
    // one is exactly what it looks like: a path this gateway does not serve. It used to answer 501
    // with `{"ok":false,"error":...}`, which claimed the operation existed and reported a shape no
    // other answer uses. It now answers 404 in the same error shape as every other routing miss,
    // instead of being relayed to the official origin (a management path is never an upstream path).
    if path.starts_with("/api/bridge/") || path.starts_with("/__bridge/") {
        return not_found(uri.path());
    }
    proxy(app, cfg, method, headers, uri, bytes).await
}
/// Inbound headers the official passthrough must not copy: hop-by-hop, the body length we
/// recompute, and this gateway's own management token.
const DROPPED_REQUEST_HEADERS: [&str; 12] = [
    "host",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-authenticate",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "x-gateway-token",
];
fn official_request(
    app: &App,
    cfg: &Value,
    method: &axum::http::Method,
    headers: &HeaderMap,
    url: String,
    bytes: &[u8],
) -> reqwest::RequestBuilder {
    let mut request = app
        .client
        .request(method.clone(), url)
        .body(bytes.to_vec())
        .timeout(Duration::from_millis(
            cfg["upstream"]["timeoutMs"].as_u64().unwrap_or(300000),
        ));
    // Node parity (proxy.js sanitizeRequestHeaders): forward every inbound name except hop-by-hop,
    // the body length we recompute, and this gateway's own management token. Accio carries its
    // session as cookie/authorization/x-accio-token on paths outside the four catalogue/generation
    // routes — for example POST /api/mcp/ali1688/sgcookie, which the 1688 workflows need — so
    // restricting the relay to those four routes makes the official origin answer 400.
    for (name, value) in headers.iter() {
        let key = name.as_str();
        if DROPPED_REQUEST_HEADERS.contains(&key) || key == "authorization" {
            continue;
        }
        request = request.header(key, value);
    }
    // A configured upstream key owns authorization; otherwise relay Accio's own credential, but
    // never one that is really this gateway's data-plane token, its admin secret or the ephemeral
    // token: relaying any of them would hand the upstream a local management credential.
    let upstream_key = cfg["upstream"]["apiKey"].as_str().unwrap_or("");
    if !upstream_key.is_empty() {
        request = request.bearer_auth(upstream_key);
    } else {
        let incoming = header(headers, "authorization");
        let gateway = cfg["server"]["token"].as_str().unwrap_or("");
        let admin = cfg["server"]["adminToken"].as_str().unwrap_or("");
        let credential = bearer_token(incoming);
        let gateway_match = !gateway.is_empty() && constant_equal(credential, gateway);
        let admin_match = !admin.is_empty() && constant_equal(credential, admin);
        let runtime_match = app
            .runtime_token
            .as_deref()
            .is_some_and(|t| constant_equal(credential, t));
        if !incoming.is_empty() && !gateway_match && !admin_match && !runtime_match {
            request = request.header("authorization", incoming);
        }
    }
    request
}
async fn proxy(
    app: &App,
    cfg: &Value,
    method: &axum::http::Method,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    bytes: &[u8],
) -> Response {
    let base = cfg["upstream"]["baseUrl"]
        .as_str()
        .unwrap()
        .trim_end_matches('/');
    let mut path = uri.path().to_string();
    // Preserve byte-exact query segments (signed URLs), removing only local auth parameters.
    let query = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|segment| {
            let key = segment.split('=').next().unwrap_or("");
            let decoded = url::form_urlencoded::parse(format!("{key}=").as_bytes())
                .next()
                .map(|(k, _)| k.into_owned())
                .unwrap_or_default();
            !["token", "sg_k"].contains(&decoded.as_str())
        })
        .collect::<Vec<_>>()
        .join("&");
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query);
    }
    let response = match official_request(app, cfg, method, headers, format!("{base}{path}"), bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return error(502, "official upstream connection failed"),
    };
    telemetry::log(
        &app.telemetry,
        "debug",
        &format!(
            "passthrough {method} {} -> {}",
            uri.path(),
            response.status().as_u16()
        ),
    );
    let generation = [
        "/api/adk/llm",
        "/api/adk/llm/generateContent",
        "/api/adk/llm/generateContent/stream",
    ]
    .contains(&canonical(uri.path()).as_str());
    if generation
        && response.status() == StatusCode::OK
        && header(response.headers(), "content-type")
            .to_ascii_lowercase()
            .contains("text/html")
    {
        let message = match bounded_text(response, 64 * 1024).await {
            Ok(text) => format!(
                "upstream gateway returned HTML instead of a model stream: {}",
                body_snippet(&text)
            ),
            Err(e) => format!("upstream gateway returned HTML instead of a model stream: {e}"),
        };
        return single_frame(stream::error_frame(&message));
    }
    let mut builder = Response::builder().status(response.status());
    let connection = response
        .headers()
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    for (key, value) in response.headers() {
        if ![
            "connection",
            "transfer-encoding",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "upgrade",
            "set-cookie",
        ]
        .contains(&key.as_str())
            && !connection.split(',').any(|v| v.trim() == key.as_str())
        {
            builder = builder.header(key, value);
        }
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap()
}
async fn bounded_text(response: reqwest::Response, max: usize) -> Result<String, String> {
    let encoding = header(response.headers(), "content-encoding").to_ascii_lowercase();
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "upstream body read failed")?;
        if bytes.len() + chunk.len() > max {
            return Err("upstream response exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let bytes = decode_bytes(&bytes, &encoding, max)?;
    String::from_utf8(bytes).map_err(|_| "invalid upstream UTF-8".into())
}
async fn bounded_json(response: reqwest::Response, max: usize) -> Result<Value, String> {
    let text = bounded_text(response, max).await?;
    serde_json::from_str(&text).map_err(|_| "invalid upstream JSON".into())
}
/// Names that introduce a credential in an upstream payload.
/// Names that introduce a credential in an upstream payload. The specific names come first so the
/// bare `key` cannot claim a value that `apiKey` introduced.
const SECRET_NAMES: [&str; 11] = [
    "apikey",
    "api_key",
    "api-key",
    "access_token",
    "client_secret",
    "authorization",
    "bearer",
    "password",
    "secret",
    "token",
    "key",
];
/// Nothing beyond this is ever looked at in an upstream body.
const EXCERPT_LIMIT: usize = 4096;

/// The token without the punctuation around it, plus where its first character sits in the token.
fn token_core(token: &str) -> (usize, &str) {
    let keep = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/');
    let trimmed = token.trim_start_matches(|c| !keep(c));
    let start = token.len() - trimmed.len();
    (start, trimmed.trim_end_matches(|c| !keep(c)))
}

/// Replace anything credential-shaped before an upstream body can reach a client.
///
/// A provider that rejects a request often quotes the credential it rejected, so its body must
/// never be relayed verbatim: this keeps the diagnosis and drops the secret. Only shapes a key
/// really has are dropped, so the provider's own wording survives.
fn scrub_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // A bare name ("Bearer", "apiKey:") says the next token is the credential itself.
    let mut after_name = false;
    for (index, token) in text.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let lower = token.to_ascii_lowercase();
        // `"apiKey":"sk-..."`, `apiKey: sk-...` and `token=...` all name the credential in the
        // same token, because whitespace is collapsed before this runs.
        let named = SECRET_NAMES.iter().find_map(|name| {
            [format!("\"{name}\""), name.to_string()]
                .iter()
                .find_map(|needle| {
                    lower.match_indices(needle.as_str()).find_map(|(at, _)| {
                        // A name inside a longer word ("monkey", "max_tokens") is not a name.
                        let boundary = at == 0
                            || !lower[..at]
                                .ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
                        let rest = &lower[at + needle.len()..];
                        let separator = rest.starts_with(':') || rest.starts_with('=');
                        (boundary && separator).then_some(at + needle.len() + 1)
                    })
                })
        });
        if let Some(value_start) = named {
            out.push_str(&token[..value_start.min(token.len())]);
            if value_start >= token.len() {
                // The value is a separate token: arm the next one.
                after_name = true;
            } else {
                out.push_str("***");
                after_name = false;
            }
            continue;
        }
        let (start, core) = token_core(token);
        let bare = core.to_ascii_lowercase();
        if SECRET_NAMES.contains(&bare.as_str()) {
            // The name of the credential is not the credential.
            out.push_str(token);
            after_name = true;
            continue;
        }
        let recognised_prefix = bare.starts_with("sk-") || bare.starts_with("sk_");
        // A long unbroken run of mixed-case letters, digits and base64 punctuation is a token, not
        // prose; a count (`max_tokens: 4096`), a URL or a plain hex digest stays readable.
        let opaque = core.len() >= 32
            && !core.contains(':')
            && core
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-+/=".contains(c))
            && core.chars().any(|c| c.is_ascii_uppercase())
            && core.chars().any(|c| c.is_ascii_lowercase())
            && core.chars().any(|c| c.is_ascii_digit());
        let secret = after_name || recognised_prefix || opaque;
        after_name = false;
        if !secret {
            out.push_str(token);
        } else if recognised_prefix {
            // Keep the prefix: it says which provider the key belongs to.
            out.push_str(&token[..(start + 3).min(token.len())]);
            out.push_str("***");
        } else {
            out.push_str("***");
        }
    }
    out
}

/// A bounded, tag-stripped, credential-free excerpt of an upstream body.
fn safe_excerpt(body: &str) -> String {
    let head: String = body.chars().take(EXCERPT_LIMIT).collect();
    body_snippet(&scrub_secrets(&head))
}

/// What a provider said went wrong, in its own words.
///
/// Providers agree on `{"error":{"message","type","code"}}`; some put those fields at the top
/// level, some make `error` a string, and Google's `code` is the HTTP status. That is enough to
/// tell a rate limit from a bad request without relaying the body itself.
struct ProviderError {
    message: String,
    kind: String,
    code: String,
    status: String,
}

/// `code` is a string in OpenAI's payload and a number in Google's.
fn provider_code(holder: &Value) -> String {
    holder["code"]
        .as_u64()
        .map(|code| code.to_string())
        .or_else(|| holder["code"].as_str().map(str::to_string))
        .unwrap_or_default()
}

impl ProviderError {
    fn parse(body: &str) -> Option<Self> {
        serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|value| Self::from_value(&value))
    }
    /// Reused by `stream::UpstreamError` for a provider error event inside an SSE stream.
    fn from_value(value: &Value) -> Option<Self> {
        // `{"error":"model not found"}` hands the string over on its own; Ollama and several
        // gateways answer that way.
        if let Some(message) = value.as_str() {
            return Some(Self {
                message: message.to_string(),
                kind: String::new(),
                code: String::new(),
                status: String::new(),
            });
        }
        let holder = match value.get("error") {
            Some(error) if error.is_object() => error,
            _ => value,
        };
        let text = |key: &str| holder[key].as_str().unwrap_or("").to_string();
        let mut error = Self {
            message: text("message"),
            kind: text("type"),
            code: provider_code(holder),
            status: text("status"),
        };
        if error.message.is_empty() {
            if let Some(text) = value["error"].as_str() {
                error.message = text.to_string();
            }
        }
        let empty = error.message.is_empty()
            && error.kind.is_empty()
            && error.code.is_empty()
            && error.status.is_empty();
        (!empty).then_some(error)
    }
    /// A single bounded, scrubbed line: `rate limit reached (type=..., code=429)`.
    fn describe(&self) -> String {
        let mut text = safe_excerpt(&self.message);
        let mut fields: Vec<String> = Vec::new();
        for (label, value) in [
            ("type", &self.kind),
            ("code", &self.code),
            ("status", &self.status),
        ] {
            let value = safe_excerpt(value);
            if !value.is_empty() {
                fields.push(format!("{label}={value}"));
            }
        }
        if !fields.is_empty() {
            let suffix = fields.join(", ");
            text = if text.is_empty() {
                format!("({suffix})")
            } else {
                format!("{text} ({suffix})")
            };
        }
        text
    }
}

/// What the client is told about a non-2xx upstream answer.
///
/// The provider's own diagnosis, the status and `Retry-After` are what make the answer actionable;
/// the body is never relayed, only a bounded and scrubbed excerpt of it. `subject` names what
/// answered, so the same wording serves the model path and the official catalogue.
fn upstream_error_message(subject: &str, status: u16, retry_after: &str, body: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let detail = ProviderError::parse(body)
        .map(|error| error.describe())
        .unwrap_or_else(|| safe_excerpt(body));
    if !detail.is_empty() {
        parts.push(detail);
    }
    let retry_after = safe_excerpt(retry_after);
    if !retry_after.is_empty() {
        parts.push(format!("retry-after={retry_after}"));
    }
    if parts.is_empty() {
        format!("{subject} returned HTTP {status}")
    } else {
        format!("{subject} returned HTTP {status}: {}", parts.join(" "))
    }
}

/// The status a client sees for a non-2xx upstream answer.
///
/// The upstream status is carried through so a caller can tell a rate limit from a bad request (and
/// read `Retry-After`). A code outside the error range — a 3xx redirect to a login page, say — has
/// no meaning as a final answer here and keeps the 502 the transport fallback uses.
fn upstream_status(status: u16) -> u16 {
    if (400..=599).contains(&status) {
        status
    } else {
        502
    }
}

/// The provider's correlation id, which is what a support ticket needs.
fn upstream_request_id(headers: &HeaderMap) -> String {
    for key in [
        "x-request-id",
        "request-id",
        "x-amzn-requestid",
        "x-correlation-id",
        "cf-ray",
    ] {
        let value = header(headers, key);
        if !value.is_empty() {
            return value.chars().take(128).collect();
        }
    }
    String::new()
}
fn body_snippet(body: &str) -> String {
    let mut source = body.to_string();
    for tag in ["script", "style"] {
        loop {
            let lower = source.to_ascii_lowercase();
            let Some(start) = lower.find(&format!("<{tag}")) else {
                break;
            };
            let end = lower[start..]
                .find(&format!("</{tag}>"))
                .map(|i| start + i + tag.len() + 3)
                .unwrap_or(source.len());
            source.replace_range(start..end, " ");
        }
    }
    let mut plain = String::new();
    let mut in_tag = false;
    for c in source.chars() {
        match c {
            '<' => {
                in_tag = true;
                plain.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => plain.push(c),
            _ => {}
        }
    }
    plain
        .replace("&nbsp;", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}
/// The embedded management panel, reused verbatim from the working build.
///
/// `no-store` matters: the panel talks to the live management API, and a cached copy would
/// keep rendering stale model lists after an edit.
fn panel_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Body::from(include_str!("dashboard.html")))
        .unwrap()
}
/// The paths that serve the static management panel (the older build used all of them). The page
/// holds no secrets, so it is also served without a token — but for GET only: any other method is a
/// 405 from `dispatch`, so a POST can never be answered with the page and never reaches the origin
/// either. Without the token exemption the page that asks for the token would itself be blocked by
/// it, which is how `server.enforceToken` used to make the panel unusable.
fn is_panel_route(path: &str) -> bool {
    ["", "/index.html", "/dashboard", "/dashboard/index.html"].contains(&path)
}
fn is_panel_path(method: &str, path: &str) -> bool {
    method == "GET" && is_panel_route(path)
}
fn single_frame(frame: Value) -> Response {
    sse_response(futures_util::stream::once(async move {
        Ok::<_, std::io::Error>(sse(&frame))
    }))
}
async fn catalog(app: &App, cfg: &Value, headers: &HeaderMap, bytes: &[u8]) -> Response {
    let mut providers = vec![];
    let enabled:Vec<_>=cfg["models"].as_array().unwrap().iter().filter(|m|m["enabled"]!=false).map(|m|json!({"modelCode":m["modelCode"],"modelName":m["modelCode"],"modelDisplayName":m["displayName"],"multimodal":m["multimodal"],"visible":m["visible"],"contextWindow":m["contextWindow"],"isDefault":m["isDefault"],"freeUse":m["freeUse"],"locked":m["locked"],"usageLevel":m["usageLevel"]})).collect();
    if cfg["catalog"]["enabled"] != false && !enabled.is_empty() {
        providers.push(json!({"provider":cfg["catalog"]["providerName"],"providerDisplayName":cfg["catalog"]["providerDisplayName"],"modelList":enabled}));
    }
    telemetry::log(
        &app.telemetry,
        "debug",
        &format!(
            "catalog: custom models={} custom_group={} official_fetch={}",
            enabled.len(),
            cfg["catalog"]["enabled"] != false && !enabled.is_empty(),
            cfg["catalog"]["fetchOfficial"] != false
        ),
    );
    let mut warning = None;
    if cfg["catalog"]["fetchOfficial"] != false {
        match official_catalog(app, cfg, headers, bytes).await {
            Ok(official) => {
                telemetry::log(
                    &app.telemetry,
                    "debug",
                    &format!("catalog: official groups={}", official.len()),
                );
                providers.extend(official)
            }
            // Say what the official catalogue answered: a bare "unavailable" hid a 401 from a
            // lapsed session and a 429 from a rate limit behind the same words.
            Err(reason) => warning = Some(format!("Official catalogue unavailable: {reason}")),
        }
    }
    json_response(json!({"success":true,"code":"200","data":providers,"warning":warning}))
}
/// How many capture files are kept when `ACCIO_BRIDGE_CAPTURE_MAX` says nothing.
const DEFAULT_CAPTURE_LIMIT: usize = 200;

/// The capture retention limit: newest files win, oldest are dropped.
fn capture_limit() -> usize {
    std::env::var("ACCIO_BRIDGE_CAPTURE_MAX")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_CAPTURE_LIMIT)
}

/// Delete the oldest captures in `dir` until at most `limit` are left.
///
/// Only this gateway's own `generate-*.json` files are candidates, so pointing the capture at a
/// directory that holds something else never deletes anything else. The recorded time decides the
/// order and the name (millisecond plus sequence) breaks a tie, which a filesystem with a coarse
/// timestamp needs.
fn prune_captures(dir: &std::path::Path, limit: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut captures: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("generate-") || !name.ends_with(".json") {
            continue;
        }
        let recorded = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        captures.push((recorded, entry.path()));
    }
    // The file about to be written takes one of the slots.
    let keep = limit.saturating_sub(1);
    if captures.len() <= keep {
        return;
    }
    captures.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let drop = captures.len() - keep;
    for (_, path) in captures.into_iter().take(drop) {
        let _ = std::fs::remove_file(path);
    }
}

/// Opt-in capture of incoming generation requests.
///
/// Setting `ACCIO_BRIDGE_CAPTURE_DIR` records each generation body there. It stays off
/// unless the variable is set, because a body holds the whole conversation; point it at a
/// scratch directory and delete it afterwards. At most `ACCIO_BRIDGE_CAPTURE_MAX` files (200 by
/// default) are kept, oldest first, so a capture left switched on cannot fill the disk it points
/// at.
async fn capture_request(telemetry: &telemetry::Shared, body: &Value) {
    let Ok(dir) = std::env::var("ACCIO_BRIDGE_CAPTURE_DIR") else {
        return;
    };
    if dir.trim().is_empty() {
        return;
    }
    // The name has to stay unique when two requests arrive in the same millisecond: a bare
    // timestamp silently overwrote the earlier body, which made the capture useless exactly when
    // it was needed (a burst of parallel turns).
    static CAPTURE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = CAPTURE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = PathBuf::from(&dir).join(format!("generate-{stamp}-{seq}.json"));
    let shown = path.display().to_string();
    let content = body.to_string();
    let limit = capture_limit();
    // The body can be `maxBodyBytes` (25 MiB by default) and this is a plain file system call:
    // it belongs on the blocking pool, not on the worker thread that other turns share.
    let written = tokio::task::spawn_blocking(move || {
        let folder = PathBuf::from(&dir);
        std::fs::create_dir_all(&folder)?;
        prune_captures(&folder, limit);
        std::fs::write(&path, content)
    })
    .await
    .unwrap_or_else(|_| Err(std::io::Error::other("capture task failed")));
    match written {
        Ok(()) => telemetry::log(
            telemetry,
            "info",
            &format!("captured generation request -> {shown}"),
        ),
        // Capture is a debugging aid, so a failure is reported and never fails the turn.
        Err(e) => telemetry::log(
            telemetry,
            "warn",
            &format!("could not capture generation request: {e}"),
        ),
    }
}
/// Log the tool set forwarded upstream, once per distinct set.
///
/// A model that cannot see a tool it needs re-issues whatever call it can, which shows up
/// as an endless loop of identical successful calls; recording the forwarded names makes
/// that verifiable instead of guessed, and flags declarations that had to be sent with a
/// permissive schema.
fn log_forwarded_tools(app: &App, report: &protocol::ToolReport, model_id: &str) {
    let signature = if report.names.is_empty() {
        "none".to_string()
    } else {
        report.names.join(",")
    };
    {
        let mut last = app.last_tools.lock().unwrap();
        if last.as_deref() == Some(signature.as_str()) {
            return;
        }
        *last = Some(signature);
    }
    if report.names.is_empty() {
        telemetry::log(
            &app.telemetry,
            "warn",
            &format!("tools forwarded for {model_id}: none"),
        );
        return;
    }
    telemetry::log(
        &app.telemetry,
        "info",
        &format!(
            "tools forwarded for {model_id}: {} [{}]",
            report.names.len(),
            report.names.join(", ")
        ),
    );
    if !report.dropped.is_empty() {
        telemetry::log(
            &app.telemetry,
            "warn",
            &format!(
                "tool declarations dropped (unrecognised shape): {}",
                report.dropped.join("; ")
            ),
        );
    }
    if !report.schema_fallbacks.is_empty() {
        telemetry::log(
            &app.telemetry,
            "warn",
            &format!(
                "tools sent with a permissive schema: {}",
                report.schema_fallbacks.join(", ")
            ),
        );
    }
    if !report.upgraded.is_empty() {
        telemetry::log(
            &app.telemetry,
            "info",
            &format!(
                "tool schemas upgraded from a placeholder declaration: {}",
                report.upgraded.join(", ")
            ),
        );
    }
}
/// The HTTP client a model endpoint deserves.
///
/// A model served from this machine (Ollama, llama.cpp, kobold.cpp) is not on the internet, so it
/// uses the client that ignores the environment proxy; every remote provider keeps honouring it.
fn model_client<'a>(app: &'a App, model: &Value) -> &'a reqwest::Client {
    let loopback = model["baseUrl"]
        .as_str()
        .and_then(|base| url::Url::parse(base).ok())
        .and_then(|url| url.host_str().map(config::is_loopback))
        .unwrap_or(false);
    if loopback {
        &app.local_client
    } else {
        &app.client
    }
}
async fn generate(app: &App, cfg: &Value, model: &Value, body: &Value) -> Response {
    let mut turn = telemetry::Turn::new(
        app.telemetry.clone(),
        model["id"].as_str().unwrap_or("unknown").to_string(),
    );
    let ollama = model["provider"] == "ollama";
    let (payload, report) = match protocol::request_with_report(body, model, ollama) {
        Ok(pair) => pair,
        Err(e) => return error(400, &e),
    };
    capture_request(&app.telemetry, body).await;
    log_forwarded_tools(app, &report, model["id"].as_str().unwrap_or("unknown"));
    let streaming = payload["stream"] != false;
    let thoughts_enabled = protocol::thoughts_requested(body);
    let endpoint = format!(
        "{}/{}",
        model["baseUrl"].as_str().unwrap().trim_end_matches('/'),
        if ollama {
            "api/chat"
        } else {
            "chat/completions"
        }
    );
    telemetry::log(
        &app.telemetry,
        "debug",
        &format!(
            "generate {} -> {} (provider={}, stream={}, tools={}, auth={})",
            model["id"].as_str().unwrap_or("?"),
            endpoint,
            model["provider"].as_str().unwrap_or("openai"),
            streaming,
            report.names.len(),
            if model["apiKey"].as_str().is_some_and(|k| !k.is_empty()) {
                "key"
            } else {
                "none"
            }
        ),
    );
    // Debug: what the client feeds back from the previous turn. A workflow loop shows up here as
    // the turn count and which tools returned, and the fed-back results are what a failing tool
    // step looks like from the gateway side.
    if let Some(contents) = body["contents"].as_array() {
        let mut fed_back: Vec<String> = Vec::new();
        for entry in contents {
            if let Some(parts) = entry["parts"].as_array() {
                for part in parts {
                    if let Some(name) = part["functionResponse"]["name"].as_str() {
                        fed_back.push(name.to_string());
                    }
                }
            }
        }
        if !fed_back.is_empty() {
            telemetry::log(
                &app.telemetry,
                "debug",
                &format!("tool results fed back: {}", fed_back.join(", ")),
            );
        }
    }
    // A model hosted on this machine gets the client that ignores the environment proxy: a proxy
    // configured for the internet must not swallow a request to 127.0.0.1.
    let mut request = model_client(app, model)
        .post(endpoint)
        .json(&payload)
        .timeout(Duration::from_millis(
            cfg["upstream"]["timeoutMs"].as_u64().unwrap_or(300000),
        ));
    let key = model["apiKey"].as_str().unwrap_or("");
    if !key.is_empty() {
        if model["provider"] == "azure" {
            request = request.header("api-key", key);
        } else {
            request = request.bearer_auth(key);
        }
    }
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            telemetry::log(
                &app.telemetry,
                "debug",
                &format!("upstream connection failed: {e}"),
            );
            // 502 stays the transport fallback: nothing upstream ever answered.
            return error(502, "model upstream connection failed");
        }
    };
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after = header(response.headers(), "retry-after").to_string();
        let request_id = upstream_request_id(response.headers());
        // "returned HTTP 429" cannot tell a rate limit from a rejected key, so the provider's own
        // error payload is read (bounded) before the response is consumed. The body itself stays
        // here: only a scrubbed excerpt of it reaches the caller or the log.
        // An unreadable or oversized error body is not worth failing over: the status and the
        // headers above already say what happened.
        let body: String = bounded_text(response, 64 * 1024).await.unwrap_or_default();
        let message = upstream_error_message("model upstream", status, &retry_after, &body);
        telemetry::log(
            &app.telemetry,
            "warn",
            &format!(
                "model upstream {} replied status={status} requestId={} retryAfter={}",
                model["provider"].as_str().unwrap_or("openai"),
                if request_id.is_empty() {
                    "-"
                } else {
                    request_id.as_str()
                },
                if retry_after.is_empty() {
                    "-"
                } else {
                    retry_after.as_str()
                },
            ),
        );
        return error(upstream_status(status), &message);
    }
    let content_type = header(response.headers(), "content-type").to_ascii_lowercase();
    telemetry::log(
        &app.telemetry,
        "debug",
        &format!(
            "upstream replied status={} contentType={}",
            response.status().as_u16(),
            content_type
        ),
    );
    if !streaming || (!ollama && !content_type.contains("text/event-stream")) {
        let text = match bounded_text(response, 8 * 1024 * 1024).await {
            Ok(text) => text,
            Err(e) => return single_frame(stream::error_frame(&e)),
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                return single_frame(stream::error_frame(&format!(
                    "upstream returned {} instead of a model response: {}",
                    if content_type.is_empty() {
                        "a non-SSE body"
                    } else {
                        &content_type
                    },
                    body_snippet(&text)
                )))
            }
        };
        return match protocol::response(&value, ollama) {
            Ok(mut frame) => {
                // An upstream that stopped early produces an error frame here: keep the reason and
                // the usage it reported, count the turn as failed, and do not overwrite it with a
                // generic "no visible content" message.
                if frame.get("error").is_some() {
                    return single_frame(frame);
                }
                if !thoughts_enabled {
                    if let Some(parts) = frame["content"]["parts"].as_array_mut() {
                        parts.retain(|part| part["thought"] != true);
                        if parts.is_empty() {
                            return single_frame(stream::error_frame(
                                "upstream returned no visible content after thinking was disabled",
                            ));
                        }
                    }
                }
                turn.success();
                single_frame(frame)
            }
            Err(e) => single_frame(stream::error_frame(&e)),
        };
    }
    // A provider that compresses its event stream has to be decoded before the frame splitter sees
    // it; otherwise valid SSE arrives as "invalid upstream UTF-8" and the turn is abandoned.
    let encoding = header(response.headers(), "content-encoding").to_ascii_lowercase();
    let mut body = match stream::BodyDecoder::new(&encoding) {
        Ok(body) => body,
        Err(e) => return single_frame(stream::error_frame(&e)),
    };
    let output = async_stream::stream! {
      let mut upstream=response.bytes_stream();let mut decoder=stream::Decoder::new(ollama);let mut sink=stream::Sink::new(thoughts_enabled);let mut failed=false;
      while let Some(chunk)=upstream.next().await{
        let encoded=match chunk{Ok(bytes)=>bytes,Err(_)=>{yield Ok::<_,std::io::Error>(sse(&stream::error_frame("upstream stream read failed")));failed=true;break;}};
        let decoded=match body.write(&encoded){Ok(decoded)=>decoded,Err(e)=>{yield Ok(sse(&stream::error_frame(&e)));failed=true;break;}};
        let events=match decoder.feed(&decoded){Ok(events)=>events,Err(e)=>{yield Ok(sse(&stream::error_frame(&e)));failed=true;break;}};
        for event in events{match sink.event(&event,ollama){Ok(frames)=>{for frame in frames{yield Ok(sse(&frame));}},Err(e)=>{yield Ok(sse(&stream::error_frame_with(e.code,&e.message)));failed=true;break;}}if sink.terminal{break;}}
        if failed||sink.terminal{break;}
      }
      if !failed&&!sink.terminal{
        let tail=body.finish();
        if !tail.is_empty(){match decoder.feed(&tail){Ok(events)=>for event in events{match sink.event(&event,ollama){Ok(frames)=>{for frame in frames{yield Ok(sse(&frame));}},Err(e)=>{yield Ok(sse(&stream::error_frame_with(e.code,&e.message)));failed=true;break;}}},Err(e)=>{yield Ok(sse(&stream::error_frame(&e)));failed=true;}}}
        if !failed{match decoder.eof(){Ok(events)=>for event in events{match sink.event(&event,ollama){Ok(frames)=>for frame in frames{yield Ok(sse(&frame));},Err(e)=>{yield Ok(sse(&stream::error_frame_with(e.code,&e.message)));failed=true;break;}}},Err(e)=>{yield Ok(sse(&stream::error_frame(&e)));failed=true;}}}
      }
      if !failed{let frame=match sink.finish(){Ok(v)=>{if v.get("error").is_none(){turn.success();}v},Err(e)=>stream::error_frame(&e)};yield Ok(sse(&frame));}
    };
    sse_response(output)
}
fn sse(v: &Value) -> String {
    format!("data: {v}\n\n")
}
fn sse_response<S>(stream: S) -> Response
where
    S: futures_util::Stream<Item = Result<String, std::io::Error>> + Send + 'static,
{
    Response::builder()
        .header("content-type", "text/event-stream; charset=utf-8")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// The configuration as a response body may carry it. Neither secret is serialised: `server.token`
/// and `server.adminToken` never appear here (or in any other payload), only the fact that a
/// second secret exists, which the panel uses to word its token prompt.
fn safe_config(c: &Value) -> Value {
    json!({
        "server": {
            "maxBodyBytes": c["server"]["maxBodyBytes"],
            "cors": c["server"]["cors"],
            "hasAdminToken": !c["server"]["adminToken"].as_str().unwrap_or("").is_empty()
        },
        "upstream": {"baseUrl": c["upstream"]["baseUrl"], "timeoutMs": c["upstream"]["timeoutMs"]},
        "catalog": {
            "enabled": c["catalog"]["enabled"],
            "fetchOfficial": c["catalog"]["fetchOfficial"],
            "providerName": c["catalog"]["providerName"],
            "providerDisplayName": c["catalog"]["providerDisplayName"],
            "timeoutMs": c["catalog"]["timeoutMs"]
        }
    })
}

async fn official_catalog(
    app: &App,
    cfg: &Value,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<Vec<Value>, String> {
    use sha1::{Digest, Sha1};
    let mut hash = Sha1::new();
    for value in [
        cfg["upstream"]["baseUrl"].as_str().unwrap_or(""),
        cfg["upstream"]["apiKey"].as_str().unwrap_or(""),
        header(headers, "authorization"),
        header(headers, "cookie"),
        header(headers, "x-accio-token"),
    ] {
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    hash.update(bytes);
    let key = format!("{:x}", hash.finalize());
    // A single bounded cache slot and mutex also coalesce concurrent identical requests.
    let mut cache = app.catalog_cache.lock().await;
    let ttl = Duration::from_millis(cfg["catalog"]["cacheTtlMs"].as_u64().unwrap_or(300000));
    if let Some((fingerprint, at, data)) = &*cache {
        if fingerprint == &key && at.elapsed() < ttl {
            return Ok(data.clone());
        }
    }
    let url = format!(
        "{}/api/llm/config/v2",
        cfg["upstream"]["baseUrl"]
            .as_str()
            .unwrap()
            .trim_end_matches('/')
    );
    let response = official_request(app, cfg, &axum::http::Method::POST, headers, url, bytes)
        .timeout(Duration::from_millis(
            cfg["catalog"]["timeoutMs"].as_u64().unwrap_or(8000),
        ))
        .send()
        .await
        .map_err(|_| "upstream request failed or timed out".to_string())?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after = header(response.headers(), "retry-after").to_string();
        let request_id = upstream_request_id(response.headers());
        let body: String = bounded_text(response, 64 * 1024).await.unwrap_or_default();
        telemetry::log(
            &app.telemetry,
            "warn",
            &format!(
                "official catalogue upstream replied status={status} requestId={} retryAfter={}",
                if request_id.is_empty() {
                    "-"
                } else {
                    request_id.as_str()
                },
                if retry_after.is_empty() {
                    "-"
                } else {
                    retry_after.as_str()
                },
            ),
        );
        return Err(upstream_error_message(
            "official catalogue upstream",
            status,
            &retry_after,
            &body,
        ));
    }
    let value = bounded_json(response, 4 * 1024 * 1024).await?;
    let data = value["data"]
        .as_array()
        .ok_or_else(|| "the catalogue answer had no data list".to_string())?
        .clone();
    *cache = Some((key, std::time::Instant::now(), data.clone()));
    Ok(data)
}

fn bearer_token(value: &str) -> &str {
    let mut parts = value.splitn(2, char::is_whitespace);
    if parts
        .next()
        .is_some_and(|p| p.eq_ignore_ascii_case("bearer"))
    {
        parts.next().unwrap_or("").trim()
    } else {
        ""
    }
}

#[cfg(test)]
mod route_tests {
    use super::canonical;
    use super::{Access, Scope};
    use serde_json::json;

    #[test]
    fn management_panel_is_served_by_the_gateway() {
        let response = super::panel_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        // The panel must be the real page, not a placeholder, and must carry the redaction-aware
        // key handling: an untouched key field has to omit `apiKey` on save.
        let html = include_str!("dashboard.html");
        assert!(html.contains("Accio Gateway Bridge"));
        assert!(html.contains("primeApiKeyField"));
        assert!(html.contains("apiKeyFieldDirty"));
        // Model rows used to splice the model id into inline handlers; the id now travels in data
        // attributes and every management call goes through the token-aware fetch wrapper.
        assert!(html.contains("data-model-action"));
        assert!(html.contains("data-model-toggle"));
        assert!(html.contains("bindModelTableActions"));
        assert!(
            !html.contains("onclick=\"openEditModelModal('${"),
            "the model id must not be interpolated into an inline handler"
        );
        assert!(html.contains("apiFetch"));
        assert!(html.contains("tokenBadge"));
        assert!(html.contains("x-gateway-token"));
        // The panel has to tell a 401 (no credential) from a 403 (a credential without the scope),
        // because after an admin token exists the inference token cannot open management routes.
        assert!(html.contains("Admin token required"));
    }
    #[test]
    fn the_panel_is_served_without_a_token_but_everything_else_is_not() {
        for path in ["", "/index.html", "/dashboard", "/dashboard/index.html"] {
            assert!(super::is_panel_path("GET", path), "{path} serves the panel");
            // The page is served for GET only: the path is still a panel path, so another method
            // is refused as unsupported instead of being proxied or answered with HTML.
            assert!(super::is_panel_route(path), "{path} is a panel path");
            assert!(
                !super::is_panel_path("POST", path),
                "POST {path} must not be answered with the page"
            );
        }
        for path in [
            "/api/bridge/status",
            "/healthz",
            "/api/llm/config/v2",
            "/dashboard/x",
        ] {
            assert!(!super::is_panel_path("GET", path), "{path} must stay gated");
        }
    }
    #[test]
    fn panel_paths_are_canonicalised_like_before() {
        // A root request arrives as an empty string, a trailing slash is dropped.
        assert_eq!(canonical("/"), "");
        assert_eq!(canonical("/dashboard/"), "/dashboard");
        for path in ["/", "/index.html", "/dashboard/", "/dashboard"] {
            let canonical = canonical(path);
            assert!(
                ["", "/index.html", "/dashboard", "/dashboard/index.html"]
                    .contains(&canonical.as_str()),
                "{path} -> {canonical}"
            );
        }
    }
    #[test]
    fn a_formal_id_is_never_shadowed_by_another_models_alias() {
        // The first model claims "fast" as an alias; the second model is actually called "fast".
        let models = vec![
            json!({"id":"provider-a","modelCode":"provider-a","aliases":["fast"],"enabled":true}),
            json!({"id":"fast","modelCode":"fast","enabled":true}),
        ];
        match super::route_model(&models, "fast") {
            super::RouteOutcome::Found(model, note) => {
                assert_eq!(model["id"], "fast", "the real id has to win");
                assert!(note.unwrap().contains("id"), "the choice is logged");
            }
            _ => panic!("expected the model whose id is fast"),
        }
        // Case-insensitive, and an alias still resolves when no id claims it.
        match super::route_model(&models, "PROVIDER-A") {
            super::RouteOutcome::Found(model, _) => assert_eq!(model["id"], "provider-a"),
            _ => panic!("id match must be case-insensitive"),
        }
        match super::route_model(&models, "provider-a") {
            super::RouteOutcome::Found(model, _) => assert_eq!(model["id"], "provider-a"),
            _ => panic!("unexpected"),
        }
    }
    #[test]
    fn disabled_and_unknown_models_are_told_apart() {
        let models = vec![json!({"id":"off","modelCode":"off","enabled":false})];
        match super::route_model(&models, "off") {
            super::RouteOutcome::Disabled(id) => assert_eq!(id, "off"),
            _ => panic!("a registered but disabled model must not look unknown"),
        }
        match super::route_model(&models, "qwen-max") {
            super::RouteOutcome::Unknown => {}
            _ => panic!("an official model name must stay unknown so it can be proxied"),
        }
        assert!(matches!(
            super::route_model(&models, "   "),
            super::RouteOutcome::Unknown
        ));
    }
    #[test]
    fn upstream_model_names_never_outrank_a_real_id() {
        let models = vec![
            json!({"id":"a","upstreamModel":"shared","enabled":true}),
            json!({"id":"shared","modelCode":"shared","enabled":true}),
        ];
        match super::route_model(&models, "shared") {
            super::RouteOutcome::Found(model, _) => assert_eq!(model["id"], "shared"),
            _ => panic!("expected the model whose id is shared"),
        }
    }
    #[test]
    fn html_snippet_omits_scripts_and_limits_unicode_safely() {
        assert_eq!(super::body_snippet("<html><script>secret()</script><style>hidden</style><h1>Gateway &nbsp; unavailable</h1></html>"), "Gateway unavailable");
        assert_eq!(super::body_snippet(&"你".repeat(500)).chars().count(), 300);
    }
    #[test]
    fn decompression_limits_expanded_bodies() {
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&vec![b'x'; 4096]).unwrap();
        let bytes = gzip.finish().unwrap();
        assert_eq!(
            super::decode_bytes(&bytes, "gzip", 100).unwrap_err(),
            "decoded body too large"
        );
        assert_eq!(
            super::decode_bytes(&bytes, "gzip", 4096).unwrap().len(),
            4096
        );
        let mut raw =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        raw.write_all(b"raw deflate").unwrap();
        assert_eq!(
            super::decode_bytes(&raw.finish().unwrap(), "deflate", 100).unwrap(),
            b"raw deflate"
        );
    }
    #[test]
    fn legacy_management_routes_are_canonicalized() {
        for (legacy, current) in [
            ("/__bridge/status", "/api/bridge/status"),
            ("/__bridge/accio", "/api/bridge/accio"),
            ("/__bridge/accio/launch", "/api/bridge/accio/launch"),
            ("/__bridge/api/models/delete", "/api/bridge/models/delete"),
        ] {
            assert_eq!(canonical(legacy), current);
            assert_eq!(canonical(&format!("{legacy}/")), current);
        }
        assert_eq!(
            canonical("/__bridge/accio/unknown"),
            "/__bridge/accio/unknown"
        );
    }
    // ---- capability model ------------------------------------------------------------------
    fn headers(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }
    /// `token` is the data-plane credential and `admin` the optional second secret. `enforceToken`
    /// is on, which is the deployment the split exists for.
    fn credentials(token: &str, admin: &str) -> serde_json::Value {
        let mut c = crate::config::defaults();
        c["server"]["token"] = json!(token);
        c["server"]["adminToken"] = json!(admin);
        c["server"]["enforceToken"] = json!(true);
        // Mutations must not touch the disk while a test runs.
        c["disableDiskSave"] = json!(true);
        crate::config::normalize(c).unwrap()
    }
    fn test_app(cfg: serde_json::Value, runtime_token: Option<&str>) -> super::App {
        super::App {
            config: std::sync::Arc::new(super::RwLock::new(cfg)),
            path: std::sync::Arc::new(std::path::PathBuf::from("unused-test-config.json")),
            client: reqwest::Client::new(),
            // The same split the real `App` uses: loopback probes must not honour the proxy.
            local_client: super::discovery::client().unwrap(),
            runtime_token: runtime_token.map(str::to_string),
            telemetry: Default::default(),
            catalog_cache: Default::default(),
            last_tools: Default::default(),
        }
    }
    fn request(
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> axum::extract::Request {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:8080")
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("x-gateway-token", token);
        }
        builder
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }
    /// One request through the real `handle` entry point, from a loopback peer.
    async fn call(
        app: &super::App,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (axum::http::StatusCode, String) {
        let response = super::handle(
            axum::extract::State(app.clone()),
            axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 41234))),
            request(method, path, token, body),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }
    #[test]
    fn one_token_configuration_still_holds_every_scope() {
        let c = credentials("inference-secret", "");
        let token = headers(&[("x-gateway-token", "inference-secret")]);
        let bearer = headers(&[("authorization", "Bearer inference-secret")]);
        for scope in [
            Scope::Inference,
            Scope::Catalogue,
            Scope::Management,
            Scope::Discovery,
            Scope::Launch,
        ] {
            assert_eq!(
                super::authorize(&token, &c, scope, false),
                Access::Granted,
                "{}",
                scope.label()
            );
            assert_eq!(
                super::authorize(&bearer, &c, scope, false),
                Access::Granted,
                "{}",
                scope.label()
            );
        }
        // A missing or wrong credential is refused exactly as before.
        assert_eq!(
            super::authorize(&headers(&[]), &c, Scope::Management, false),
            Access::Unauthorized
        );
        assert_eq!(
            super::authorize(
                &headers(&[("x-gateway-token", "wrong")]),
                &c,
                Scope::Management,
                false
            ),
            Access::Unauthorized
        );
    }
    #[test]
    fn a_second_secret_takes_the_privileged_scopes_from_the_inference_token() {
        let c = credentials("inference-secret", "admin-secret");
        let inference = headers(&[("x-gateway-token", "inference-secret")]);
        assert_eq!(
            super::authorize(&inference, &c, Scope::Inference, false),
            Access::Granted
        );
        assert_eq!(
            super::authorize(&inference, &c, Scope::Catalogue, false),
            Access::Granted
        );
        for scope in [Scope::Management, Scope::Discovery, Scope::Launch] {
            assert_eq!(
                super::authorize(&inference, &c, scope, false),
                Access::Forbidden,
                "{}",
                scope.label()
            );
        }
        for admin in [
            headers(&[("x-gateway-token", "admin-secret")]),
            headers(&[("authorization", "Bearer admin-secret")]),
        ] {
            for scope in [
                Scope::Inference,
                Scope::Catalogue,
                Scope::Management,
                Scope::Discovery,
                Scope::Launch,
            ] {
                assert_eq!(
                    super::authorize(&admin, &c, scope, false),
                    Access::Granted,
                    "{}",
                    scope.label()
                );
            }
        }
        // Configuring the second secret is what makes the privileged scopes privileged: they are
        // authenticated even on loopback with `enforceToken` off, while the data plane stays open.
        let mut relaxed = c.clone();
        relaxed["server"]["enforceToken"] = json!(false);
        assert_eq!(
            super::authorize(&headers(&[]), &relaxed, Scope::Inference, false),
            Access::Granted
        );
        assert_eq!(
            super::authorize(&headers(&[]), &relaxed, Scope::Management, false),
            Access::Unauthorized
        );
        assert_eq!(
            super::authorize(&inference, &relaxed, Scope::Management, false),
            Access::Forbidden
        );
        // ... and with no admin token there is nothing to separate, so nothing is newly required.
        let mut single = credentials("inference-secret", "");
        single["server"]["enforceToken"] = json!(false);
        assert_eq!(
            super::authorize(&headers(&[]), &single, Scope::Launch, false),
            Access::Granted
        );
    }
    #[test]
    fn routes_are_grouped_by_the_capability_they_need() {
        for path in [
            "/api/bridge/status",
            "/api/bridge/config",
            "/api/bridge/models",
            "/api/bridge/usage",
            "/api/bridge/logs",
            "/api/bridge/providers",
        ] {
            assert_eq!(
                super::required_scope("GET", path),
                Scope::Management,
                "GET {path}"
            );
        }
        for (method, path) in [
            ("PUT", "/api/bridge/config"),
            ("POST", "/api/bridge/models"),
            ("DELETE", "/api/bridge/models"),
            ("POST", "/api/bridge/models/delete"),
            ("PATCH", "/api/bridge/models/one"),
            ("DELETE", "/api/bridge/models/one"),
            ("POST", "/api/bridge/providers"),
            ("PATCH", "/api/bridge/providers/p_1"),
            ("DELETE", "/api/bridge/providers/p_1"),
            ("POST", "/api/bridge/logging"),
            ("GET", "/api/bridge/from-the-future"),
            ("GET", "/__bridge/unknown"),
        ] {
            assert_eq!(
                super::required_scope(method, path),
                Scope::Management,
                "{method} {path}"
            );
        }
        for path in [
            "/api/bridge/discover",
            "/api/bridge/discover/models",
            "/api/bridge/accio",
        ] {
            assert_eq!(
                super::required_scope("GET", path),
                Scope::Discovery,
                "{path}"
            );
        }
        assert_eq!(
            super::required_scope("POST", "/api/bridge/accio/launch"),
            Scope::Launch
        );
        for path in [
            "/healthz",
            "/__bridge/health",
            "/api/adk/llm",
            "/api/adk/llm/generateContent",
            "/api/bridge/test",
            "/api/mcp/ali1688/sgcookie",
        ] {
            assert_eq!(
                super::required_scope("POST", path),
                Scope::Inference,
                "{path}"
            );
        }
        assert_eq!(super::required_scope("GET", "/healthz"), Scope::Inference);
        assert_eq!(
            super::required_scope("GET", "/__bridge/health"),
            Scope::Inference
        );
        assert_eq!(
            super::required_scope("POST", "/api/llm/config/v2"),
            Scope::Catalogue
        );
    }
    #[tokio::test]
    async fn a_second_secret_splits_the_management_plane_off_the_data_plane() {
        use axum::http::StatusCode;
        let app = test_app(credentials("inference-secret", "admin-secret"), None);
        // The inference credential still drives the data plane...
        assert_eq!(
            call(&app, "GET", "/healthz", Some("inference-secret"), "")
                .await
                .0,
            StatusCode::OK
        );
        // ...but no longer the management plane. The refusal is a 403 and not a 401: the caller is
        // authenticated, it simply lacks the scope, and the panel words its prompt on that.
        for path in [
            "/api/bridge/status",
            "/api/bridge/config",
            "/api/bridge/models",
            "/api/bridge/logs",
            "/api/bridge/usage",
            "/api/bridge/providers",
            "/api/bridge/logging",
        ] {
            assert_eq!(
                call(&app, "GET", path, Some("inference-secret"), "")
                    .await
                    .0,
                StatusCode::FORBIDDEN,
                "GET {path}"
            );
        }
        for (method, path) in [
            ("PUT", "/api/bridge/config"),
            ("POST", "/api/bridge/models"),
            ("PATCH", "/api/bridge/models/one"),
            ("DELETE", "/api/bridge/models?id=one"),
        ] {
            assert_eq!(
                call(&app, method, path, Some("inference-secret"), "")
                    .await
                    .0,
                StatusCode::FORBIDDEN,
                "{method} {path}"
            );
        }
        // Discovery and launch are capabilities of their own; the empty launch body proves the
        // request never reaches the launcher.
        assert_eq!(
            call(
                &app,
                "GET",
                "/api/bridge/discover",
                Some("inference-secret"),
                ""
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &app,
                "GET",
                "/api/bridge/accio",
                Some("inference-secret"),
                ""
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &app,
                "POST",
                "/api/bridge/accio/launch",
                Some("inference-secret"),
                ""
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        // No credential at all is a 401, so the two rejections stay distinguishable.
        assert_eq!(
            call(&app, "GET", "/api/bridge/status", None, "").await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&app, "POST", "/api/bridge/accio/launch", None, "")
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        // The admin secret opens every one of them.
        assert_eq!(
            call(&app, "GET", "/api/bridge/status", Some("admin-secret"), "")
                .await
                .0,
            StatusCode::OK
        );
        // The panel page keeps being served to a caller without any credential.
        let (status, body) = call(&app, "GET", "/dashboard", None, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Accio Gateway Bridge"));
    }
    #[tokio::test]
    async fn the_single_token_and_ephemeral_desktop_flows_behave_as_before() {
        use axum::http::StatusCode;
        let app = test_app(credentials("inference-secret", ""), None);
        for path in [
            "/api/bridge/status",
            "/api/bridge/config",
            "/api/bridge/models",
            "/api/bridge/logs",
            "/api/bridge/usage",
            "/api/bridge/providers",
            "/api/bridge/logging",
        ] {
            assert_eq!(
                call(&app, "GET", path, Some("inference-secret"), "")
                    .await
                    .0,
                StatusCode::OK,
                "GET {path}"
            );
        }
        // The launch route is reached (its empty body is a 400), i.e. the only credential still
        // holds the launch scope.
        assert_eq!(
            call(
                &app,
                "POST",
                "/api/bridge/accio/launch",
                Some("inference-secret"),
                ""
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        // `ACCIO_BRIDGE_TOKEN` replaces the saved token for the management paths, exactly as the
        // desktop shell relies on.
        let runtime = test_app(credentials("inference-secret", ""), Some("runtime-secret"));
        assert_eq!(
            call(
                &runtime,
                "GET",
                "/api/bridge/status",
                Some("runtime-secret"),
                ""
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &runtime,
                "GET",
                "/api/bridge/status",
                Some("inference-secret"),
                ""
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        // Once an admin token exists, the ephemeral token keeps the data plane (health check,
        // inference) and loses management/discovery/launch: it does not silently gain privileges,
        // and a shell that needs them sends the admin secret instead.
        let split = test_app(
            credentials("inference-secret", "admin-secret"),
            Some("runtime-secret"),
        );
        assert_eq!(
            call(&split, "GET", "/healthz", Some("runtime-secret"), "")
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &split,
                "GET",
                "/api/bridge/status",
                Some("runtime-secret"),
                ""
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &split,
                "GET",
                "/api/bridge/status",
                Some("admin-secret"),
                ""
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    #[tokio::test]
    async fn no_response_body_ever_carries_a_configured_secret() {
        use axum::http::StatusCode;
        let cfg = credentials("inference-secret", "admin-secret");
        let app = test_app(cfg.clone(), Some("runtime-secret"));
        let safe = super::safe_config(&cfg).to_string();
        assert_eq!(
            super::safe_config(&cfg)["server"]["hasAdminToken"],
            json!(true),
            "the panel may learn that a second secret exists"
        );
        for secret in ["admin-secret", "inference-secret", "runtime-secret"] {
            assert!(!safe.contains(secret), "safe_config exposed {secret}");
        }
        for (method, path, body) in [
            ("GET", "/healthz", ""),
            ("GET", "/api/bridge/status", ""),
            ("GET", "/api/bridge/config", ""),
            ("GET", "/api/bridge/models", ""),
            ("GET", "/api/bridge/providers", ""),
            ("GET", "/api/bridge/usage", ""),
            ("GET", "/api/bridge/logs", ""),
            ("GET", "/api/bridge/logging", ""),
            ("GET", "/", ""),
            (
                "POST",
                "/api/bridge/models",
                r#"{"id":"leak","provider":"openai","baseUrl":"http://127.0.0.1:9/v1","apiKey":"model-secret"}"#,
            ),
        ] {
            let (status, text) = call(&app, method, path, Some("admin-secret"), body).await;
            assert_eq!(status, StatusCode::OK, "{method} {path}: {text}");
            for secret in [
                "admin-secret",
                "inference-secret",
                "runtime-secret",
                "model-secret",
            ] {
                assert!(
                    !text.contains(secret),
                    "{method} {path} echoed {secret}: {text}"
                );
            }
        }
    }
    #[tokio::test]
    async fn the_model_relay_refuses_a_host_the_owner_did_not_allow() {
        use axum::http::StatusCode;
        let app = test_app(credentials("inference-secret", "admin-secret"), None);
        // An external host, with a caller-supplied key in the body: refused before any socket is
        // opened, and the refusal names the host, never the URL or the key.
        let (status, text) = call(
            &app,
            "POST",
            "/api/bridge/discover/models",
            Some("admin-secret"),
            r#"{"baseUrl":"http://example.com:9999/v1","apiKey":"relayed-secret"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
        assert!(text.contains("server.discoveryAllowedHosts"), "{text}");
        assert!(!text.contains("relayed-secret"), "{text}");
        assert!(!text.contains("example.com:9999/v1"), "{text}");
        // A loopback host is still probed, so the panel's local detection keeps working.
        let (status, text) = call(
            &app,
            "POST",
            "/api/bridge/discover/models",
            Some("admin-secret"),
            r#"{"baseUrl":"http://127.0.0.1:9/v1"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(
            !text.contains("discoveryAllowedHosts"),
            "a loopback probe must not be refused by the host gate: {text}"
        );
    }
    #[test]
    fn upstream_errors_keep_the_status_and_the_provider_message() {
        // The provider's own diagnosis and the retry hint are what the caller needs; both used to
        // be thrown away in favour of a flat "returned HTTP 429".
        let message = super::upstream_error_message(
            "model upstream",
            429,
            "3",
            r#"{"error":{"message":"Rate limit reached for gpt","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
        );
        assert!(
            message.starts_with("model upstream returned HTTP 429:"),
            "{message}"
        );
        assert!(message.contains("Rate limit reached for gpt"), "{message}");
        assert!(message.contains("type=rate_limit_error"), "{message}");
        assert!(message.contains("code=rate_limit_exceeded"), "{message}");
        assert!(message.contains("retry-after=3"), "{message}");
        // A body that is not JSON still contributes a bounded excerpt.
        let message = super::upstream_error_message(
            "model upstream",
            403,
            "",
            "<html><h1>Blocked</h1></html>",
        );
        assert_eq!(message, "model upstream returned HTTP 403: Blocked");
        assert_eq!(
            super::upstream_error_message("official catalogue upstream", 500, "", ""),
            "official catalogue upstream returned HTTP 500"
        );
        // 502 stays the transport fallback: only a status a client can act on is passed through.
        assert_eq!(super::upstream_status(429), 429);
        assert_eq!(super::upstream_status(500), 500);
        assert_eq!(super::upstream_status(302), 502);
        assert_eq!(super::upstream_status(999), 502);
    }
    #[test]
    fn error_excerpts_are_bounded_and_never_relay_a_credential() {
        let body = r#"{"error":{"message":"invalid key sk-live-ABCDEFGHIJKLMNOP supplied","authorization":"Bearer sk-live-ABCDEFGHIJKLMNOP","apiKey":"sk-live-ABCDEFGHIJKLMNOP"}}"#;
        let message = super::upstream_error_message("model upstream", 401, "", body);
        assert!(!message.contains("ABCDEFGHIJKLMNOP"), "{message}");
        assert!(message.contains("sk-***"), "{message}");
        assert!(message.contains("invalid key"), "{message}");
        assert!(super::scrub_secrets("token=abcdefghijklmnopqrstuvwxyz012345").contains("***"));
        // A long opaque token is dropped, while ordinary prose is left alone.
        assert_eq!(
            super::scrub_secrets("aZ09secretVALUEwithLENGTH1234567890"),
            "***"
        );
        assert_eq!(
            super::scrub_secrets("the model refused the request"),
            "the model refused the request"
        );
        // Nothing beyond the excerpt limit is even looked at.
        let huge = format!("{} {}", "x".repeat(8192), "sk-live-ABCDEFGHIJKLMNOP");
        assert!(!super::safe_excerpt(&huge).contains("sk-live"));
    }
    #[test]
    fn capture_retention_drops_the_oldest_files_only() {
        let dir = std::env::temp_dir().join(format!("accio-capture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A foreign file in the same directory has to survive the pruning.
        std::fs::write(dir.join("notes.txt"), "keep me").unwrap();
        for index in 0..5u64 {
            std::fs::write(
                dir.join(format!("generate-170000000000{index}-0.json")),
                "{}",
            )
            .unwrap();
        }
        super::prune_captures(&dir, 3);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "generate-1700000000003-0.json",
                "generate-1700000000004-0.json",
                "notes.txt"
            ]
        );
        assert!(super::capture_limit() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_tier_whose_hits_are_all_disabled_never_falls_through_to_a_lower_tier() {
        // The name resolves in the `id` tier, where its only hit is switched off, while another
        // model offers the same name as an alias. Answering from the alias would send the request to
        // a provider the caller never named, which is the confusion the tier order exists to remove.
        let models = vec![
            json!({"id":"off","modelCode":"off","enabled":false}),
            json!({"id":"b2","modelCode":"b2","aliases":["off"],"enabled":true}),
        ];
        match super::route_model(&models, "off") {
            super::RouteOutcome::Disabled(id) => assert_eq!(id, "off"),
            _ => panic!("a name whose hits are all disabled must be refused, not rerouted"),
        }
        // The same rule holds for a lower tier: a disabled alias is refused instead of being
        // answered by another model's upstream-model name.
        let deeper = vec![
            json!({"id":"hidden","modelCode":"hidden","aliases":["shared-name"],"enabled":false}),
            json!({"id":"other","modelCode":"other","upstreamModel":"shared-name","enabled":true}),
        ];
        match super::route_model(&deeper, "shared-name") {
            super::RouteOutcome::Disabled(id) => assert_eq!(id, "hidden"),
            _ => panic!("the alias tier must be refused as disabled"),
        }
    }
    #[tokio::test]
    async fn an_unknown_management_path_is_a_404_in_the_shared_error_shape() {
        use axum::http::StatusCode;
        let app = test_app(credentials("inference-secret", ""), None);
        for (method, path) in [
            ("GET", "/api/bridge/nonexistent-probe"),
            ("POST", "/api/bridge/nonexistent-probe"),
            ("GET", "/api/bridge/models/1/2"),
            ("GET", "/__bridge/unknown"),
        ] {
            let (status, text) = call(&app, method, path, Some("inference-secret"), "{}").await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {text}");
            let body: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(body["status"], json!(404), "{method} {path}: {text}");
            assert_eq!(body["error"], json!("Not Found"), "{method} {path}: {text}");
            assert!(body["timestamp"].is_string(), "{method} {path}: {text}");
            assert!(body["requestId"].is_string(), "{method} {path}: {text}");
            assert_eq!(body["path"], json!(path), "{method} {path}: {text}");
            assert!(
                !text.contains("not implemented") && body.get("ok").is_none(),
                "an unknown path must not claim the operation exists: {text}"
            );
        }
    }
    #[tokio::test]
    async fn a_method_a_panel_path_does_not_accept_is_a_405_on_every_panel_path() {
        use axum::http::StatusCode;
        let app = test_app(credentials("inference-secret", ""), None);
        for path in ["/", "/index.html", "/dashboard", "/dashboard/"] {
            for method in ["POST", "PUT", "PATCH", "DELETE"] {
                let (status, text) = call(&app, method, path, Some("inference-secret"), "{}").await;
                assert_eq!(
                    status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path}: {text}"
                );
                assert!(
                    !text.contains("<!DOCTYPE html>"),
                    "{method} {path} must not be answered with the page"
                );
                let body: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(body["status"], json!(405), "{method} {path}: {text}");
                assert_eq!(
                    body["error"],
                    json!("Method Not Allowed"),
                    "{method} {path}: {text}"
                );
                assert!(body["timestamp"].is_string(), "{method} {path}: {text}");
                assert!(body["requestId"].is_string(), "{method} {path}: {text}");
            }
            // GET still serves the page, and still without a credential.
            let (status, text) = call(&app, "GET", path, None, "").await;
            assert_eq!(status, StatusCode::OK, "{path}: {text}");
            assert!(text.contains("Accio Gateway Bridge"), "{path}");
        }
    }
    #[tokio::test]
    async fn an_edit_that_was_not_written_to_disk_says_so() {
        use axum::http::StatusCode;
        // `credentials()` sets `disableDiskSave`, which used to be invisible: the panel was told
        // "ok" while nothing reached the file.
        let app = test_app(credentials("inference-secret", ""), None);
        let (status, text) = call(
            &app,
            "POST",
            "/api/bridge/models",
            Some("inference-secret"),
            r#"{"id":"memory-only","provider":"openai","baseUrl":"http://127.0.0.1:9/v1"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["persisted"], json!(false), "{text}");
        assert_eq!(body["ok"], json!(true), "{text}");
        assert_eq!(body["success"], json!(true), "{text}");
        // The edit is still applied in memory: the flag only stops the write.
        assert!(
            body["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == json!("memory-only")),
            "{text}"
        );
        let (status, text) = call(
            &app,
            "PUT",
            "/api/bridge/config",
            Some("inference-secret"),
            r#"{"catalog":{"enabled":false}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["persisted"],
            json!(false),
            "{text}"
        );
    }
}
