use serde_json::{json, Value};
use std::path::Path;

pub fn defaults() -> Value {
    json!({"server":{"host":"127.0.0.1","port":8080,"maxBodyBytes":26214400,"cors":true,"token":"","enforceToken":false},"upstream":{"baseUrl":"https://phoenix-gw.alibaba.com","timeoutMs":300000,"apiKey":""},"catalog":{"enabled":true,"fetchOfficial":true,"providerName":"custom","providerDisplayName":"Custom AI","timeoutMs":8000},"models":[]})
}
pub fn normalize(mut config: Value) -> Result<Value, String> {
    if !config.is_object() {
        return Err("configuration must be an object".into());
    }
    let base = defaults();
    for section in ["server", "upstream", "catalog"] {
        if config.get(section).is_none() {
            config[section] = base[section].clone();
        }
        if !config[section].is_object() {
            return Err(format!("{section} must be an object"));
        }
        for (key, value) in base[section].as_object().unwrap() {
            if config[section].get(key).is_none() {
                config[section][key] = value.clone();
            }
        }
    }
    if config.get("models").is_none() {
        config["models"] = json!([]);
    }
    let models = config["models"]
        .as_array_mut()
        .ok_or("models must be an array")?;
    let mut ids = std::collections::HashSet::new();
    for model in models.iter_mut() {
        *model = normalize_model(model.clone())?;
        if !ids.insert(model["id"].as_str().unwrap().to_lowercase()) {
            return Err("duplicate model id".into());
        }
    }
    if !models
        .iter()
        .any(|m| m["isDefault"] == true && m["enabled"] == true)
    {
        if let Some(m) = models.iter_mut().find(|m| m["enabled"] == true) {
            m["isDefault"] = json!(true);
        }
    }
    for (section, keys) in [
        ("server", vec!["cors", "enforceToken"]),
        ("catalog", vec!["enabled", "fetchOfficial"]),
    ] {
        for key in keys {
            if !config[section][key].is_boolean() {
                return Err(format!("{section}.{key} must be boolean"));
            }
        }
    }
    for (section, key) in [
        ("server", "maxBodyBytes"),
        ("upstream", "timeoutMs"),
        ("catalog", "timeoutMs"),
    ] {
        let n = config[section][key]
            .as_u64()
            .ok_or("size/timeout must be a positive integer")?;
        if n == 0 || n > 100 * 1024 * 1024 {
            return Err("size/timeout out of allowed range".into());
        }
    }
    if !config["server"]["port"]
        .as_u64()
        .is_some_and(|p| p <= 65535)
    {
        return Err("port must be 0..65535".into());
    }
    if !config["server"]["token"].is_string() {
        return Err("token must be a string".into());
    }
    let host = config["server"]["host"]
        .as_str()
        .ok_or("host must be a string")?;
    if !is_loopback(host) && config["server"]["token"].as_str().unwrap_or("").is_empty() {
        return Err("non-loopback bind requires a gateway token".into());
    }
    validate_url(config["upstream"]["baseUrl"].as_str().unwrap_or(""))?;
    Ok(config)
}
pub fn normalize_model(mut m: Value) -> Result<Value, String> {
    let id = m["id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("model id required")?
        .to_string();
    let provider = m["provider"].as_str().unwrap_or("openai").to_lowercase();
    if !["openai", "openai-compatible", "azure", "ollama"].contains(&provider.as_str()) {
        return Err("unsupported provider".into());
    }
    m["provider"] = json!(provider);
    let default = json!({"modelCode":id,"displayName":id,"upstreamModel":id,"baseUrl":if provider=="ollama"{"http://127.0.0.1:11434"}else{"https://api.openai.com/v1"},"apiKey":"","enabled":true,"visible":true,"stream":true,"multimodal":false,"contextWindow":128000,"isDefault":false,"freeUse":true,"locked":false,"usageLevel":0,"aliases":[]});
    for (k, v) in default.as_object().unwrap() {
        if m.get(k).is_none() {
            m[k] = v.clone();
        }
    }
    for key in [
        "enabled",
        "visible",
        "stream",
        "multimodal",
        "isDefault",
        "freeUse",
        "locked",
    ] {
        if !m[key].is_boolean() {
            return Err(format!("{key} must be boolean"));
        }
    }
    for key in ["modelCode", "displayName", "upstreamModel"] {
        if !m[key].as_str().is_some_and(|s| !s.trim().is_empty()) {
            return Err(format!("{key} must be a nonempty string"));
        }
    }
    if !m["aliases"]
        .as_array()
        .is_some_and(|a| a.iter().all(Value::is_string))
    {
        return Err("aliases must be strings".into());
    }
    if !m["contextWindow"].as_u64().is_some_and(|n| n > 0) {
        return Err("contextWindow must be positive".into());
    }
    validate_url(m["baseUrl"].as_str().unwrap_or(""))?;
    if !m["apiKey"].is_string() {
        return Err("apiKey must be a string".into());
    }
    Ok(m)
}
pub fn validate_url(s: &str) -> Result<(), String> {
    let u = url::Url::parse(s).map_err(|_| "invalid URL")?;
    if !["http", "https"].contains(&u.scheme())
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err("baseUrl requires HTTP(S), no credentials/query/fragment".into());
    }
    Ok(())
}
pub fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}
pub fn public_model(m: &Value) -> Value {
    let mut m = m.clone();
    let has = m["apiKey"].as_str().is_some_and(|k| !k.is_empty());
    m.as_object_mut().unwrap().remove("apiKey");
    m["hasKey"] = json!(has);
    m
}
pub async fn save(path: &Path, v: &Value) -> Result<(), String> {
    if v["disableDiskSave"] == true {
        return Ok(());
    }
    // Every write path (config edits, model create/update/delete) lands here, which is the only
    // place that can refuse a colliding configuration before it reaches the disk.
    if let Some(conflict) = route_name_conflict(v) {
        return Err(conflict);
    }
    let bytes = serde_json::to_vec_pretty(v).map_err(|_| "serialization failed")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = path.with_extension(format!("json.{}.{stamp}.tmp", std::process::id()));
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .await
        .map_err(|_| "cannot write config temporary file")?;
    use tokio::io::AsyncWriteExt;
    file.write_all(&bytes)
        .await
        .map_err(|_| "cannot write config")?;
    file.sync_all().await.map_err(|_| "cannot sync config")?;
    drop(file);
    tokio::fs::rename(tmp, path)
        .await
        .map_err(|_| "cannot replace config atomically")?;
    Ok(())
}

/// Name collision between models: a `modelCode` or alias that repeats another model's formal `id`
/// leaves two models answering to one name.
///
/// Routing resolves that by tier, but a configuration that creates it is almost certainly a
/// mistake, so a *persisted* save is refused while the editor is still open (`save` enforces it and
/// the write handlers report it as a 400). Loading only warns, so an already-collided file can still
/// be started and repaired instead of locking its owner out of the panel.
pub fn route_name_conflict(config: &Value) -> Option<String> {
    let models = config["models"].as_array()?;
    let mut claimed: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for model in models.iter() {
        if let Some(id) = model["id"].as_str().filter(|s| !s.is_empty()) {
            claimed.insert(id.to_lowercase(), id.to_string());
        }
    }
    for model in models.iter() {
        let owner = model["id"].as_str().unwrap_or("");
        let mut secondary: Vec<String> = Vec::new();
        if let Some(code) = model["modelCode"].as_str() {
            secondary.push(code.to_string());
        }
        if let Some(list) = model["aliases"].as_array() {
            secondary.extend(list.iter().filter_map(|a| a.as_str().map(str::to_string)));
        }
        for name in secondary {
            if let Some(other) = claimed.get(&name.to_lowercase()) {
                if !other.eq_ignore_ascii_case(owner) {
                    return Some(format!(
                        "model {owner} also answers to {name:?}, which is the id of {other}; rename one of them"
                    ));
                }
            }
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_hidden() {
        let m = normalize_model(json!({"id":"x","apiKey":"s"})).unwrap();
        assert!(public_model(&m).get("apiKey").is_none());
        assert_eq!(public_model(&m)["hasKey"], true);
    }
    #[test]
    fn remote_bind_requires_token() {
        let mut c = defaults();
        c["server"]["host"] = json!("0.0.0.0");
        assert!(normalize(c).is_err());
    }
    #[test]
    fn a_secondary_name_may_not_shadow_another_models_id() {
        let with_models = |models: Vec<Value>| {
            let mut c = defaults();
            c["models"] = json!(models);
            normalize(c).unwrap()
        };
        // Two providers, the first claiming "fast" as an alias and the second called "fast".
        let collision = with_models(vec![
            json!({"id":"provider-a","aliases":["fast"]}),
            json!({"id":"fast"}),
        ]);
        let message = route_name_conflict(&collision).unwrap();
        assert!(message.contains("also answers to \"fast\""), "{message}");
        // Loading must still succeed: refusing to start would lock the owner out of the panel that
        // could repair the collision.
        assert!(normalize(collision).is_ok());

        // Sharing an upstream model name across providers is legitimate and must stay allowed.
        let shared = with_models(vec![
            json!({"id":"a","upstreamModel":"gpt-4o"}),
            json!({"id":"b","upstreamModel":"gpt-4o"}),
        ]);
        assert_eq!(route_name_conflict(&shared), None);

        // A model repeating its own id as an alias is not a conflict.
        let own_name = with_models(vec![json!({"id":"a","modelCode":"a","aliases":["a"]})]);
        assert_eq!(route_name_conflict(&own_name), None);

        // Case-only differences are still the same name.
        let case_only = with_models(vec![json!({"id":"a","aliases":["B"]}), json!({"id":"b"})]);
        assert!(route_name_conflict(&case_only).is_some());
    }
}
