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
    client: reqwest::Client,
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
fn authorize(h: &HeaderMap, c: &Value, management: bool, remote: bool) -> bool {
    let token = c["server"]["token"].as_str().unwrap_or("");
    let required = c["server"]["enforceToken"] == true || remote;
    if !required {
        return true;
    }
    if token.is_empty() {
        return false;
    }
    let candidate = header(h, "x-gateway-token");
    let bearer = bearer_token(header(h, "authorization"));
    let _ = management;
    constant_equal(token, candidate) || constant_equal(token, bearer)
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
        println!("Accio Gateway Bridge Rust {}\nUsage: accio-gateway-bridge-rust [--config PATH] [--host IP] [--port PORT]\nACCIO_BRIDGE_CONFIG: config path; ACCIO_BRIDGE_TOKEN: ephemeral management-only token.\nDefaults to loopback:8080. Port 0 selects a free port and prints JSON readiness.\nA missing config starts with safe empty defaults; model/config edits create it.\n--version prints version. No credential discovery or model calls at startup.",env!("CARGO_PKG_VERSION"));
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
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()?;
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
    let management = path.starts_with("/api/bridge/");
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
    // the token. Every management route below it is still gated.
    if !panel
        && !authorize(
            &headers,
            &cfg,
            management,
            !peer.ip().is_loopback()
                || !config::is_loopback(cfg["server"]["host"].as_str().unwrap_or("")),
        )
    {
        return error(401, "invalid gateway token");
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
    if is_panel_path(method.as_str(), path) {
        return panel_response();
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
        if let Err(e) = config::save(&app.path, &next).await {
            return error(500, &e);
        }
        *guard = next;
        return json_response(result);
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
        if let Some(conflict) = config::route_name_conflict(&next) {
            return error(400, &conflict);
        }
        if let Err(e) = config::save(&app.path, &next).await {
            return error(500, &e);
        }
        *guard = next;
        return json_response(json!({"ok":true,"config":safe_config(&guard)}));
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
        if let Err(e) = config::save(&app.path, &next).await {
            return error(500, &e);
        }
        *guard = next;
        let mut out = result;
        out["ok"] = json!(true);
        out["success"] = json!(true);
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
        return json_response(discovery::discover(cfg, &app.client).await);
    }
    if method == "POST" && path == "/api/bridge/discover/models" {
        let body = match decode(bytes, header(headers, "content-encoding"), max) {
            Ok(v) => v,
            Err(e) => return error(400, &e),
        };
        return match discovery::list_models(cfg, &app.client, &body).await {
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
    if path.starts_with("/api/bridge/") || path.starts_with("/__bridge/") {
        return error(
            501,
            "This management operation is not implemented in Rust core v0.1",
        );
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
    // never one that is really this gateway's token.
    let upstream_key = cfg["upstream"]["apiKey"].as_str().unwrap_or("");
    if !upstream_key.is_empty() {
        request = request.bearer_auth(upstream_key);
    } else {
        let incoming = header(headers, "authorization");
        let gateway = cfg["server"]["token"].as_str().unwrap_or("");
        let credential = bearer_token(incoming);
        let gateway_match = !gateway.is_empty() && constant_equal(credential, gateway);
        let runtime_match = app
            .runtime_token
            .as_deref()
            .is_some_and(|t| constant_equal(credential, t));
        if !incoming.is_empty() && !gateway_match && !runtime_match {
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
/// Paths that serve the static management panel (the older build used all of them). The page holds
/// no secrets, so it is also served without a token: otherwise the page that asks for the token
/// would itself be blocked, which is how `server.enforceToken` used to make the panel unusable.
fn is_panel_path(method: &str, path: &str) -> bool {
    method == "GET" && ["", "/index.html", "/dashboard", "/dashboard/index.html"].contains(&path)
}
/// keep rendering stale model lists after an edit.
fn panel_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Body::from(include_str!("dashboard.html")))
        .unwrap()
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
            Err(()) => warning = Some("Official catalogue unavailable"),
        }
    }
    json_response(json!({"success":true,"code":"200","data":providers,"warning":warning}))
}
/// Opt-in capture of incoming generation requests.
///
/// Setting `ACCIO_BRIDGE_CAPTURE_DIR` records each generation body there. It stays off
/// unless the variable is set, because a body holds the whole conversation; point it at a
/// scratch directory and delete it afterwards.
fn capture_request(telemetry: &telemetry::Shared, body: &Value) {
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
    let path = PathBuf::from(dir).join(format!("generate-{stamp}-{seq}.json"));
    match path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| std::fs::write(&path, body.to_string()))
    {
        Ok(()) => telemetry::log(
            telemetry,
            "info",
            &format!("captured generation request -> {}", path.display()),
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
    capture_request(&app.telemetry, body);
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
    let mut request = app
        .client
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
            return error(502, "model upstream connection failed");
        }
    };
    if !response.status().is_success() {
        return error(
            502,
            &format!(
                "model upstream returned HTTP {}",
                response.status().as_u16()
            ),
        );
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
    let output = async_stream::stream! {
      let mut upstream=response.bytes_stream();let mut decoder=stream::Decoder::new(ollama);let mut sink=stream::Sink::new(thoughts_enabled);let mut failed=false;
      while let Some(chunk)=upstream.next().await{let events=match chunk{Ok(bytes)=>decoder.feed(&bytes),Err(_)=>Err("upstream stream read failed".into())};let events=match events{Ok(v)=>v,Err(e)=>{yield Ok::<_,std::io::Error>(sse(&stream::error_frame(&e)));failed=true;break;}};
        for event in events{match sink.event(&event,ollama){Ok(frames)=>{for frame in frames{yield Ok(sse(&frame));}},Err(e)=>{yield Ok(sse(&stream::error_frame(&e)));failed=true;break;}}if sink.terminal{break;}}
        if failed||sink.terminal{break;}
      }
      if !failed&&!sink.terminal{match decoder.eof(){Ok(events)=>for event in events{match sink.event(&event,ollama){Ok(frames)=>for frame in frames{yield Ok(sse(&frame));},Err(e)=>{yield Ok(sse(&stream::error_frame(&e)));failed=true;break;}}},Err(e)=>{yield Ok(sse(&stream::error_frame(&e)));failed=true;}}}
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

fn safe_config(c: &Value) -> Value {
    json!({"server":{"maxBodyBytes":c["server"]["maxBodyBytes"],"cors":c["server"]["cors"]},"upstream":{"baseUrl":c["upstream"]["baseUrl"],"timeoutMs":c["upstream"]["timeoutMs"]},"catalog":{"enabled":c["catalog"]["enabled"],"fetchOfficial":c["catalog"]["fetchOfficial"],"providerName":c["catalog"]["providerName"],"providerDisplayName":c["catalog"]["providerDisplayName"],"timeoutMs":c["catalog"]["timeoutMs"]}})
}

async fn official_catalog(
    app: &App,
    cfg: &Value,
    headers: &HeaderMap,
    bytes: &[u8],
) -> Result<Vec<Value>, ()> {
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
        .map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let value = bounded_json(response, 4 * 1024 * 1024)
        .await
        .map_err(|_| ())?;
    let data = value["data"].as_array().ok_or(())?.clone();
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
    }
    #[test]
    fn the_panel_is_served_without_a_token_but_everything_else_is_not() {
        for path in ["", "/index.html", "/dashboard", "/dashboard/index.html"] {
            assert!(super::is_panel_path("GET", path), "{path} serves the panel");
            // The Node build serves the page for GET only; a POST used to be answered with HTML.
            assert!(
                !super::is_panel_path("POST", path),
                "POST {path} must be proxied"
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
}
