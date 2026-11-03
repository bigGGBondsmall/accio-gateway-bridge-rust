//! Explicit, bounded local discovery. No probes run until a caller requests them.
use futures_util::{stream, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const LIMIT: usize = 2 * 1024 * 1024;
pub fn timestamp() -> String {
    // RFC3339 UTC, without a clock/date dependency
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86400) as i64;
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs % 86400 / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}
pub fn client() -> Result<Client, String> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| "HTTP client initialization failed".into())
}
pub fn parse_models(kind: &str, data: &Value) -> Option<Vec<String>> {
    if kind == "koboldcpp" {
        return data["result"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(|s| vec![s.trim().rsplit('/').next().unwrap_or(s).to_string()]);
    }
    let (field, key) = if kind == "ollama" {
        ("models", "name")
    } else {
        ("data", "id")
    };
    Some(
        data[field]
            .as_array()?
            .iter()
            .filter_map(|m| m.as_str().or_else(|| m[key].as_str()))
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}
async fn fetch_json(client: &Client, url: &str, key: &str, ms: u64) -> Result<Value, String> {
    let mut request = client
        .get(url)
        .header("accept", "application/json")
        .timeout(Duration::from_millis(ms.clamp(1, 30000)));
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|_| "upstream request failed or timed out".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "upstream returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let mut bytes = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| "upstream body interrupted")?;
        if chunk.len() > LIMIT - bytes.len() {
            return Err("upstream response exceeded size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "upstream returned invalid JSON".into())
}
pub async fn list_models_for(
    client: &Client,
    provider: &str,
    base: &str,
    key: &str,
    timeout_ms: u64,
) -> Result<Vec<String>, String> {
    let parsed = url::Url::parse(base).map_err(|_| "baseUrl must be a valid http(s) URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(
            "baseUrl must be a valid http(s) URL without credentials/query/fragment".into(),
        );
    }
    let endpoint = format!(
        "{}{}",
        base.trim_end_matches('/'),
        if provider == "ollama" {
            "/api/tags"
        } else {
            "/models"
        }
    );
    let data = fetch_json(client, &endpoint, key, timeout_ms).await?;
    parse_models(provider, &data)
        .ok_or_else(|| "upstream returned an unexpected response shape".into())
}
#[derive(Clone, Debug)]
pub struct Probe {
    pub port: u16,
    pub kind: String,
    pub name: String,
    pub provider: String,
    pub path: String,
    pub base_path: String,
}
pub fn default_probes() -> Vec<Probe> {
    let mut v = vec![
        (11434, "ollama", "Ollama"),
        (1234, "lmstudio", "LM Studio"),
        (8000, "vllm", "vLLM"),
        (8080, "llamacpp", "llama.cpp"),
        (5001, "koboldcpp", "KoboldCpp"),
        (8081, "localai", "LocalAI"),
        (1337, "jan", "Jan"),
        (5000, "textgen", "Text Generation WebUI"),
    ];
    for p in [11435, 1235, 8001, 8082, 5002, 1338] {
        v.push((p, "openai", "OpenAI-compatible"));
    }
    v.into_iter()
        .map(|(port, kind, name)| Probe {
            port,
            kind: kind.into(),
            name: name.into(),
            provider: if kind == "ollama" { "ollama" } else { "openai" }.into(),
            path: match kind {
                "ollama" => "/api/tags",
                "koboldcpp" => "/api/v1/model",
                _ => "/v1/models",
            }
            .into(),
            base_path: if kind == "ollama" { "" } else { "/v1" }.into(),
        })
        .collect()
}
pub fn annotate(services: &mut [Value], models: &[Value]) {
    for s in services {
        let found: Vec<Value> = s["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|name| {
                models.iter().any(|m| {
                    m["provider"] == s["provider"]
                        && m["baseUrl"].as_str().unwrap_or("").trim_end_matches('/')
                            == s["baseUrl"].as_str().unwrap_or("").trim_end_matches('/')
                        && ["id", "modelCode", "upstreamModel"]
                            .iter()
                            .any(|k| m[*k] == **name)
                })
            })
            .cloned()
            .collect();
        s["alreadyAdded"] = json!(found);
    }
}
pub async fn discover(cfg: &Value, _client: &Client) -> Value {
    let client = match client() {
        Ok(c) => c,
        Err(e) => return json!({"ok":false,"error":e,"services":[]}),
    };
    let models = cfg["models"].as_array().cloned().unwrap_or_default();
    let mut out = discover_probes(
        &client,
        default_probes(),
        &[cfg["server"]["port"].as_u64().unwrap_or(8080) as u16],
        &models,
        700,
    )
    .await;
    out["ok"] = json!(true);
    out["cached"] = json!(false);
    out
}
pub async fn list_models(
    cfg: &Value,
    _client: &Client,
    body: &Value,
) -> Result<Value, (u16, String)> {
    let mut options = body.clone();
    if let Some(id) = body["providerId"].as_str().filter(|s| !s.is_empty()) {
        let groups = crate::management::providers(cfg);
        let group = groups.iter().find(|g| g["id"] == id);
        let ids: Vec<&str> = group
            .into_iter()
            .flat_map(|g| g["models"].as_array().into_iter().flatten())
            .filter_map(|m| m["id"].as_str())
            .collect();
        let canonical = cfg["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["id"].as_str().is_some_and(|id| ids.contains(&id)));
        match canonical {
            Some(m) => options = m.clone(),
            None => return Ok(json!({"ok":false,"models":[],"error":"provider not found"})),
        };
    }
    if options.get("apiKey").is_some_and(|v| !v.is_string()) {
        return Err((400, "apiKey must be a string".into()));
    }
    let base = options["baseUrl"]
        .as_str()
        .ok_or((400, "baseUrl must be a valid http(s) URL".into()))?;
    let c = client().map_err(|e| (500, e))?;
    match list_models_for(
        &c,
        options["provider"].as_str().unwrap_or("openai"),
        base,
        options["apiKey"].as_str().unwrap_or(""),
        10000,
    )
    .await
    {
        Ok(models) => Ok(json!({"ok":true,"models":models})),
        Err(e) if e.starts_with("baseUrl") => Err((400, e)),
        Err(e) => Ok(json!({"ok":false,"error":e,"models":[]})),
    }
}
pub async fn discover_probes(
    client: &Client,
    probes: Vec<Probe>,
    skip_ports: &[u16],
    models: &[Value],
    timeout_ms: u64,
) -> Value {
    let mut services:Vec<Value>=stream::iter(probes.into_iter().take(64).filter(|p|p.port!=0&&!skip_ports.contains(&p.port))).map(|p|async move{
        let started=Instant::now(); let root=format!("http://127.0.0.1:{}",p.port);
        let data=fetch_json(client,&format!("{}{}",root,p.path),"",timeout_ms).await.ok()?;
        let names=parse_models(&p.kind,&data)?;
        Some(json!({"id":format!("{}@127.0.0.1:{}",p.kind,p.port),"host":"127.0.0.1","port":p.port,"kind":p.kind,"name":p.name,"provider":p.provider,"baseUrl":format!("{}{}",root,p.base_path),"models":names,"alreadyAdded":[],"latencyMs":started.elapsed().as_millis()}))
    }).buffer_unordered(4).filter_map(|v|async{v}).collect().await;
    services.sort_by_key(|s| (s["kind"] == "openai", s["port"].as_u64().unwrap_or(0)));
    let mut seen = HashSet::new();
    services.retain(|s| seen.insert(s["port"].as_u64()));
    services.sort_by_key(|s| s["port"].as_u64());
    annotate(&mut services, models);
    json!({"scannedAt":timestamp(),"services":services})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shapes_and_annotation() {
        assert_eq!(
            parse_models("ollama", &json!({"models":[{"name":"a"},"b",{}]})),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(parse_models("openai", &json!({})), None);
        assert_eq!(
            parse_models("koboldcpp", &json!({"result":"path/model"})),
            Some(vec!["model".into()])
        );
        let mut s =
            vec![json!({"provider":"openai","baseUrl":"http://localhost/v1","models":["a","b"]})];
        annotate(
            &mut s,
            &[json!({"provider":"openai","baseUrl":"http://localhost/v1/","upstreamModel":"a"})],
        );
        assert_eq!(s[0]["alreadyAdded"], json!(["a"]));
    }
    #[tokio::test]
    async fn fixture_probe() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut b = [0; 2048];
            let count = s.read(&mut b).await.unwrap();
            assert!(count > 0);
            let body = r#"{"data":[{"id":"fixture-model"}]}"#;
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        });
        let p = Probe {
            port,
            kind: "openai".into(),
            name: "fixture".into(),
            provider: "openai".into(),
            path: "/v1/models".into(),
            base_path: "/v1".into(),
        };
        let result = discover_probes(&client().unwrap(), vec![p], &[], &[], 1000).await;
        assert_eq!(result["services"][0]["models"], json!(["fixture-model"]));
        task.await.unwrap();
    }
    #[tokio::test]
    async fn reject_credential_url() {
        assert!(list_models_for(
            &client().unwrap(),
            "openai",
            "https://name:secret@example.com/v1",
            "",
            10
        )
        .await
        .is_err());
    }
    #[tokio::test]
    async fn redirect_never_forwards_credentials() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = source.local_addr().unwrap().port();
        let target_port = target.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut socket, _) = source.accept().await.unwrap();
            let mut request = [0; 2048];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/models\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let result = list_models_for(
            &client().unwrap(),
            "openai",
            &format!("http://127.0.0.1:{port}"),
            "fixture-secret",
            500,
        )
        .await;
        assert_eq!(result.unwrap_err(), "upstream returned HTTP 302");
        assert!(
            tokio::time::timeout(Duration::from_millis(30), target.accept())
                .await
                .is_err()
        );
        task.await.unwrap();
    }
}
