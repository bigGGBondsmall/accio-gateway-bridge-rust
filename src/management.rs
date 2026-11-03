//! Local provider/model administration. All writes validate a private copy, persist,
//! and only then publish it under the shared configuration write lock.
use crate::config;
use axum::http::Method;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

type Failure = (u16, String);
type Result<T> = std::result::Result<T, Failure>;
fn bad(message: &str) -> Failure {
    (400, message.into())
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn base(v: &Value) -> &str {
    text(v, "baseUrl").trim_end_matches('/')
}
fn group_id(model: &Value) -> String {
    // Node contract: opaque SHA-1 prefix, not an authentication primitive.
    let material = format!(
        "{}|{}|{}",
        text(model, "provider"),
        base(model),
        text(model, "apiKey")
    );
    let digest = Sha1::digest(material.as_bytes());
    format!(
        "p_{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}
fn display_name(m: &Value) -> String {
    if text(m, "provider") == "ollama" {
        return "Ollama".into();
    }
    let url = url::Url::parse(base(m)).ok();
    let host = url.as_ref().and_then(|u| u.host_str()).unwrap_or("OpenAI");
    if matches!(host, "127.0.0.1" | "localhost")
        && url.as_ref().and_then(|u| u.port()) == Some(1234)
    {
        return "LM Studio".into();
    }
    match host {
        "api.openai.com" => "OpenAI".into(),
        "api.deepseek.com" => "DeepSeek".into(),
        "openrouter.ai" => "OpenRouter".into(),
        "api.siliconflow.cn" => "硅基流动".into(),
        "dashscope.aliyuncs.com" => "通义千问".into(),
        "api.moonshot.cn" => "Moonshot".into(),
        "open.bigmodel.cn" => "智谱".into(),
        _ => url
            .as_ref()
            .and_then(|u| u.port())
            .map(|p| format!("{host}:{p}"))
            .unwrap_or_else(|| host.into()),
    }
}
pub fn providers(config: &Value) -> Vec<Value> {
    let mut groups: Vec<(String, Vec<&Value>)> = vec![];
    for model in config["models"].as_array().into_iter().flatten() {
        let id = group_id(model);
        if let Some((_, models)) = groups.iter_mut().find(|(_, models)| {
            text(models[0], "provider") == text(model, "provider")
                && base(models[0]) == base(model)
                && text(models[0], "apiKey") == text(model, "apiKey")
        }) {
            models.push(model);
        } else {
            groups.push((id, vec![model]));
        }
    }
    groups.into_iter().map(|(id, models)| {
        let first = models[0];
        let has_key = !text(first,"apiKey").is_empty();
        let enabled = models.iter().filter(|m| m["enabled"] != false).count();
        let local = url::Url::parse(base(first)).ok().and_then(|u| u.host_str().map(config::is_loopback)).unwrap_or(false);
        json!({"id":id,"provider":first["provider"],"name":display_name(first),"baseUrl":base(first),
            "apiKeyMasked":if has_key {"••••"} else {""},"hasKey":has_key,"local":local,
            "enabled":enabled>0,"modelCount":models.len(),"enabledCount":enabled,
            "models":models.into_iter().map(config::public_model).collect::<Vec<_>>()})
    }).collect()
}
fn validate_patch(body: &Value, allowed: &[&str]) -> Result<()> {
    let object = body
        .as_object()
        .ok_or_else(|| bad("request must be an object"))?;
    for (key, value) in object {
        if !allowed.contains(&key.as_str()) {
            return Err(bad(&format!("unsupported field: {key}")));
        }
        match key.as_str() {
            "enabled" | "visible" | "isDefault" | "multimodal" | "stream"
                if !value.is_boolean() =>
            {
                return Err(bad("model flags must be boolean"))
            }
            "contextWindow" => {
                let number = value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()));
                if !number.is_some_and(|n| n > 0) {
                    return Err(bad("contextWindow must be positive"));
                }
            }
            "models" if !value.is_array() => return Err(bad("models must be an array")),
            "provider" | "baseUrl" | "apiKey" | "displayName" | "upstreamModel" | "id"
                if !value.is_string() =>
            {
                return Err(bad("model text fields must be strings"))
            }
            _ => {}
        }
    }
    if let Some(url) = body.get("baseUrl") {
        config::validate_url(url.as_str().unwrap_or("")).map_err(|e| bad(&e))?;
    }
    Ok(())
}
fn find_group(config: &Value, id: &str) -> Result<Value> {
    config["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| group_id(m) == id)
        .cloned()
        .ok_or((404, "provider not found".into()))
}
fn destination_guard(old: &Value, body: &Value) -> Result<()> {
    if !text(old, "apiKey").is_empty()
        && body.get("apiKey").is_none()
        && (body
            .get("baseUrl")
            .is_some_and(|v| v.as_str().unwrap_or("").trim_end_matches('/') != base(old))
            || body.get("provider").is_some_and(|v| v != &old["provider"]))
    {
        return Err(bad(
            "Changing provider or base URL requires explicitly replacing or clearing API key",
        ));
    }
    Ok(())
}
fn add_models(config: &mut Value, body: &Value, canonical: &Value) -> Result<String> {
    let entries = body["models"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| bad("models must be a non-empty array"))?;
    let mut seen = std::collections::HashSet::new();
    let mut prepared = vec![];
    for entry in entries {
        validate_patch(
            entry,
            &[
                "id",
                "displayName",
                "upstreamModel",
                "contextWindow",
                "multimodal",
            ],
        )?;
        let id = text(entry, "id").trim();
        if id.is_empty() || !seen.insert(id.to_lowercase()) {
            return Err(bad("model IDs must be nonempty and unique"));
        }
        let mut model = entry.clone();
        model["id"] = json!(id);
        for key in ["provider", "baseUrl", "apiKey"] {
            model[key] = canonical[key].clone();
        }
        // Group insertion explicitly opts into its credential; do not retain keys from another group.
        let existing = config["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| text(m, "id").eq_ignore_ascii_case(id));
        if let Some(old) = existing {
            model["isDefault"] = old["isDefault"].clone();
            model["aliases"] = old["aliases"].clone();
        }
        if let Some(n) = model["contextWindow"]
            .as_str()
            .and_then(|v| v.parse::<u64>().ok())
        {
            model["contextWindow"] = json!(n);
        }
        prepared.push(config::normalize_model(model).map_err(|e| bad(&e))?);
    }
    let list = config["models"].as_array_mut().unwrap();
    for model in prepared {
        if let Some(index) = list
            .iter()
            .position(|m| text(m, "id").eq_ignore_ascii_case(text(&model, "id")))
        {
            list[index] = model;
        } else {
            list.push(model);
        }
    }
    Ok(group_id(canonical))
}
fn decoded_segment(raw: &str) -> Result<String> {
    // URL path segments, unlike form data, preserve literal plus signs.
    let encoded = format!("id={}", raw.replace('+', "%2B").replace('&', "%26"));
    let decoded = url::form_urlencoded::parse(encoded.as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    if decoded.is_empty() {
        Err(bad("identifier required"))
    } else {
        Ok(decoded)
    }
}
/// Delete one model by identifier, case-insensitively.
fn delete_model(config: &mut Value, id: &str) -> Result<Value> {
    let list = config["models"].as_array_mut().unwrap();
    let before = list.len();
    list.retain(|m| !text(m, "id").eq_ignore_ascii_case(id));
    if list.len() == before {
        return Err((404, format!("model \"{id}\" not found")));
    }
    Ok(json!({"deleted":id}))
}
fn mutate_inner(config: &mut Value, path: &str, method: &Method, body: &Value) -> Result<Value> {
    if let Some(raw) = path.strip_prefix("/api/bridge/models/") {
        let id = decoded_segment(raw)?;
        validate_patch(
            body,
            &[
                "enabled",
                "visible",
                "isDefault",
                "multimodal",
                "stream",
                "displayName",
                "upstreamModel",
                "contextWindow",
                "locked",
                "freeUse",
            ],
        )?;
        if method == Method::DELETE {
            return delete_model(config, &id);
        }
        let list = config["models"].as_array_mut().unwrap();
        let index = list
            .iter()
            .position(|m| text(m, "id").eq_ignore_ascii_case(&id))
            .ok_or((404, "model not found".into()))?;
        for (key, value) in body.as_object().unwrap() {
            // Compatibility: catalog entitlement flags are intentionally not editable.
            if matches!(key.as_str(), "locked" | "freeUse") {
                continue;
            }
            if matches!(key.as_str(), "displayName" | "upstreamModel")
                && value.as_str().unwrap().trim().is_empty()
            {
                continue;
            }
            list[index][key] = if key == "contextWindow" && value.is_string() {
                json!(value.as_str().unwrap().parse::<u64>().unwrap())
            } else {
                value.clone()
            };
        }
        if body["isDefault"] == true {
            for (i, m) in list.iter_mut().enumerate() {
                m["isDefault"] = json!(i == index);
            }
        }
        return Ok(
            json!({"ok":true,"model":config::public_model(&list[index]),"models":list.iter().map(config::public_model).collect::<Vec<_>>() }),
        );
    }
    let target_id;
    if path == "/api/bridge/providers" {
        validate_patch(body, &["provider", "baseUrl", "apiKey", "models"])?;
        let provider = text(body, "provider").to_lowercase();
        if !["openai", "ollama"].contains(&provider.as_str()) {
            return Err(bad("provider must be openai or ollama"));
        }
        config::validate_url(text(body, "baseUrl")).map_err(|e| bad(&e))?;
        let canonical =
            json!({"provider":provider,"baseUrl":base(body),"apiKey":text(body,"apiKey")});
        target_id = Some(add_models(config, body, &canonical)?);
    } else {
        let raw = path
            .strip_prefix("/api/bridge/providers/")
            .ok_or_else(|| bad("invalid provider path"))?;
        let (raw, append) = raw
            .strip_suffix("/models")
            .map(|r| (r, true))
            .unwrap_or((raw, false));
        let id = decoded_segment(raw)?;
        let old = find_group(config, &id)?;
        if append {
            validate_patch(body, &["models"])?;
            target_id = Some(add_models(config, body, &old)?);
        } else if method == Method::DELETE {
            let list = config["models"].as_array_mut().unwrap();
            let removed: Vec<_> = list
                .iter()
                .filter(|m| group_id(m) == id)
                .map(|m| m["id"].clone())
                .collect();
            list.retain(|m| group_id(m) != id);
            return Ok(json!({"ok":true,"deleted":removed,"providers":providers(config)}));
        } else {
            validate_patch(body, &["baseUrl", "apiKey", "enabled"])?;
            destination_guard(&old, body)?;
            let mut canonical = old.clone();
            for (key, value) in body.as_object().unwrap() {
                canonical[key] = value.clone();
            }
            for model in config["models"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .filter(|m| group_id(m) == id)
            {
                for (key, value) in body.as_object().unwrap() {
                    model[key] = value.clone();
                }
            }
            target_id = Some(group_id(&canonical));
        }
    }
    let groups = providers(config);
    let selected = groups
        .iter()
        .find(|g| Some(text(g, "id")) == target_id.as_deref())
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({"ok":true,"provider":selected,"providers":groups}))
}
pub fn mutate(config: &mut Value, method: &str, path: &str, body: &Value) -> Result<Value> {
    let method = Method::from_bytes(method.as_bytes()).map_err(|_| bad("invalid method"))?;
    let allowed = (path == "/api/bridge/providers" && method == Method::POST)
        || (path.starts_with("/api/bridge/models/")
            && (method == Method::PATCH || method == Method::DELETE))
        || (path.starts_with("/api/bridge/providers/")
            && (if path.ends_with("/models") {
                method == Method::POST
            } else {
                method == Method::PATCH || method == Method::DELETE
            }));
    if !allowed {
        return Err((405, "method not allowed".into()));
    }
    mutate_inner(config, path, &method, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        config::normalize(json!({"models":[{"id":"a","apiKey":"secret-123"},{"id":"b","apiKey":"secret-123"},{"id":"c","apiKey":"other"}]})).unwrap()
    }
    #[test]
    fn grouping_and_redaction() {
        let c = fixture();
        let g = providers(&c);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0]["id"], "p_afa645e3");
        assert_eq!(g[0]["modelCount"], 2);
        assert!(!serde_json::to_string(&g).unwrap().contains("secret-123"));
    }
    #[test]
    fn group_patch_clear_and_destination_guard() {
        let mut c = fixture();
        let id = providers(&c)[0]["id"].as_str().unwrap().to_owned();
        let p = format!("/api/bridge/providers/{id}");
        assert!(mutate_inner(
            &mut c,
            &p,
            &Method::PATCH,
            &json!({"baseUrl":"https://different.example/v1"})
        )
        .is_err());
        assert_eq!(c["models"][0]["baseUrl"], "https://api.openai.com/v1");
        mutate_inner(
            &mut c,
            &p,
            &Method::PATCH,
            &json!({"baseUrl":"https://different.example/v1","apiKey":""}),
        )
        .unwrap();
        assert_eq!(c["models"][0]["apiKey"], "");
        assert_eq!(c["models"][1]["apiKey"], "");
    }
    #[test]
    fn patch_default_exclusive() {
        let mut c = fixture();
        mutate_inner(
            &mut c,
            "/api/bridge/models/a",
            &Method::PATCH,
            &json!({"isDefault":true}),
        )
        .unwrap();
        mutate_inner(
            &mut c,
            "/api/bridge/models/b",
            &Method::PATCH,
            &json!({"isDefault":true,"contextWindow":"4096"}),
        )
        .unwrap();
        assert_eq!(c["models"][0]["isDefault"], false);
        assert_eq!(c["models"][1]["isDefault"], true);
        assert_eq!(c["models"][1]["contextWindow"], 4096);
    }
    #[test]
    fn append_uses_group_key_not_client_key() {
        let mut c = fixture();
        let id = providers(&c)[0]["id"].as_str().unwrap().to_owned();
        let p = format!("/api/bridge/providers/{id}/models");
        mutate_inner(&mut c, &p, &Method::POST, &json!({"models":[{"id":"new"}]})).unwrap();
        assert_eq!(c["models"][3]["apiKey"], "secret-123");
        assert!(mutate_inner(
            &mut c,
            &p,
            &Method::POST,
            &json!({"models":[{"id":"oops","apiKey":"injected"}]})
        )
        .is_err());
    }
    #[test]
    fn catalog_flags_cannot_be_patched() {
        let mut c = fixture();
        mutate_inner(
            &mut c,
            "/api/bridge/models/a",
            &Method::PATCH,
            &json!({"locked":true,"freeUse":false}),
        )
        .unwrap();
        assert_eq!(c["models"][0]["locked"], false);
        assert_eq!(c["models"][0]["freeUse"], true);
    }
    #[test]
    fn invalid_batch_has_no_partial_write() {
        let mut c = fixture();
        let before = c.clone();
        assert!(mutate_inner(&mut c,"/api/bridge/providers",&Method::POST,&json!({"provider":"openai","baseUrl":"http://localhost:1234/v1","models":[{"id":"new"},{"id":""}]})).is_err());
        assert_eq!(c, before);
    }
    #[test]
    fn path_delete_removes_one_model_and_reports_missing() {
        let mut c = fixture();
        assert_eq!(
            mutate(&mut c, "DELETE", "/api/bridge/models/A", &json!({})).unwrap(),
            json!({"deleted":"A"})
        );
        assert!(!c["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "a"));
        assert_eq!(
            mutate(&mut c, "DELETE", "/api/bridge/models/a", &json!({}))
                .unwrap_err()
                .0,
            404
        );
        // A delete ignores any body rather than validating it as a patch.
        assert_eq!(
            mutate(
                &mut c,
                "DELETE",
                "/api/bridge/models/b",
                &json!({"displayName":""})
            )
            .unwrap(),
            json!({"deleted":"b"})
        );
        // PATCH on the legacy delete route targets a model literally named "delete".
        assert_eq!(
            mutate(&mut c, "PATCH", "/api/bridge/models/delete", &json!({}))
                .unwrap_err()
                .0,
            404
        );
        assert_eq!(
            mutate(&mut c, "PUT", "/api/bridge/models/c", &json!({}))
                .unwrap_err()
                .0,
            405
        );
        assert_eq!(
            mutate(&mut c, "DELETE", "/api/bridge/models/", &json!({}))
                .unwrap_err()
                .0,
            400
        );
    }
}
