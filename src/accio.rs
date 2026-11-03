//! Local Accio metadata inspection and explicit launch. Never reads login tokens.
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
fn read_small(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?.take(1024 * 1024 + 1);
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    if text.len() > 1024 * 1024 {
        None
    } else {
        Some(text)
    }
}
fn read_json(path: &Path) -> Value {
    read_small(path)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}
fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}
fn candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let mut out = Vec::new();
        for var in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(var) {
                let root = PathBuf::from(root);
                out.push(if var == "LOCALAPPDATA" {
                    root.join("Programs/Accio/Accio.exe")
                } else {
                    root.join("Accio/Accio.exe")
                });
            }
        }
        out.push(home().join("AppData/Local/Programs/Accio/Accio.exe"));
        out
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/Applications/Accio.app/Contents/MacOS/Accio"),
            home().join("Applications/Accio.app/Contents/MacOS/Accio"),
        ]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        vec![
            PathBuf::from("/usr/bin/accio"),
            PathBuf::from("/opt/Accio/accio"),
        ]
    }
}
pub fn inspect_at(paths: &[PathBuf], accio_home: &Path, pids: &[u32]) -> Value {
    let found = paths.iter().find(|p| p.is_file());
    let mut result = json!({"installed":found.is_some(),"edition":"none","editionLabel":"未安装","path":null,"version":null,"evidence":[],"running":!pids.is_empty(),"runningPids":pids,"gatewayPort":null,"gatewayUrl":null,"signedIn":false,"accountCount":0,"detectedAt":crate::discovery::timestamp(),"warning":"在标准安装路径未找到 Accio 客户端","downloadUrls":{"cn":"https://www.accio-ai.com/work","global":"https://www.accio.com/work"}});
    let Some(path) = found else { return result };
    let install = path.parent().unwrap_or(Path::new(""));
    #[cfg(target_os = "macos")]
    let resources = install.parent().unwrap_or(install).join("Resources");
    #[cfg(not(target_os = "macos"))]
    let resources = install.join("resources");
    let domain = read_json(&accio_home.join("domain-config.json"));
    let update = read_small(&resources.join("app-update.yml")).unwrap_or_default();
    let mut evidence = Vec::new();
    let (mut cn, mut global) = (0, 0);
    let mut add = |source: &str, field: &str, value: &str, edition: &str, weight: i32| {
        if edition == "cn" {
            cn += weight;
        } else if edition == "global" {
            global += weight;
        }
        evidence.push(json!({"source":source,"field":field,"value":value,"targetEdition":edition,"weight":weight}));
    };
    if let Some(region) = domain["region"].as_str() {
        match region {
            "CN" => add("domain-config.json", "region", region, "cn", 100),
            "GLOBAL" => add("domain-config.json", "region", region, "global", 100),
            _ => {}
        }
    }
    if let Some(raw) = domain
        .pointer("/services/client/domain")
        .and_then(Value::as_str)
    {
        let url = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        if let Ok(url) = url::Url::parse(&url) {
            if let Some(host) = url.host_str() {
                if host == "accio-ai.com" || host.ends_with(".accio-ai.com") {
                    add(
                        "domain-config.json",
                        "services.client.domain",
                        host,
                        "cn",
                        70,
                    );
                } else if host == "accio.com" || host.ends_with(".accio.com") {
                    add(
                        "domain-config.json",
                        "services.client.domain",
                        host,
                        "global",
                        70,
                    );
                }
            }
        }
    }
    for (needle, edition, weight) in [
        ("work-download.accio-ai.com", "cn", 80),
        ("work-download.accio.com", "global", 80),
        ("ALIBABA (CHINA)", "cn", 90),
    ] {
        if update.contains(needle) {
            add(
                "app-update.yml",
                "publisher/update",
                needle,
                edition,
                weight,
            );
        }
    }
    if let Some(text) = read_small(&accio_home.join("appInstall.jsonl")) {
        let mut has_cn = false;
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                has_cn |= v["region"] == "CN";
            }
        }
        if has_cn {
            add("appInstall.jsonl", "region", "CN", "cn", 60);
        }
    }
    let (edition, label) = if cn > global && cn >= 60 {
        ("cn", "Accio Work 中国版")
    } else if global > cn && global >= 60 {
        ("global", "Accio Work 国际版")
    } else {
        ("unknown", "未知发行版本")
    };
    let version = update
        .lines()
        .find_map(|l| l.trim().strip_prefix("version:").map(str::trim))
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(str::to_owned)
        .or_else(|| {
            let v = read_json(&install.join("install-channel.json"));
            let name = v["filename"].as_str()?;
            let start = name.to_ascii_lowercase().find("accio-")? + 6;
            let version: String = name[start..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if version.is_empty() {
                None
            } else {
                Some(version)
            }
        });
    let account_count = std::fs::read_dir(accio_home.join("accounts"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name() != "guest" && e.file_type().is_ok_and(|t| t.is_dir()))
                .count()
        })
        .unwrap_or(0);
    // Only runtime gateway metadata; never read login/auth files or return account IDs.
    if let Ok(entries) = std::fs::read_dir(accio_home.join("accounts")) {
        for entry in entries.flatten().take(128) {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let gateway = read_json(&entry.path().join(".accio/runtime/gateway-cli.json"));
            if let Some(raw) = gateway["url"].as_str() {
                if let Ok(url) = url::Url::parse(raw) {
                    if matches!(url.scheme(), "http" | "https")
                        && url.host_str().is_some_and(crate::config::is_loopback)
                        && url.username().is_empty()
                        && url.password().is_none()
                    {
                        result["gatewayUrl"] = json!(url.origin().ascii_serialization());
                        result["gatewayPort"] = json!(url.port_or_known_default());
                        break;
                    }
                }
            }
        }
    }
    result["edition"] = json!(edition);
    result["editionLabel"] = json!(label);
    result["path"] = json!(path);
    result["installDir"] = json!(install);
    result["version"] = json!(version);
    result["scores"] = json!({"cn":cn,"global":global});
    result["evidence"] = json!(evidence);
    result["accountCount"] = json!(account_count);
    result["signedIn"] = json!(account_count > 0);
    result["warning"] = if edition == "unknown" {
        json!("未能明确判定发行版本；启动需要 force 确认")
    } else {
        Value::Null
    };
    result
}
async fn running_pids() -> Vec<u32> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = tokio::process::Command::new("tasklist");
        c.args(["/FI", "IMAGENAME eq Accio.exe", "/FO", "CSV", "/NH"]);
        c
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("pgrep");
        c.args(["-ix", "accio"]);
        c
    };
    cmd.kill_on_drop(true)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = match tokio::time::timeout(Duration::from_millis(1500), cmd.output()).await {
        Ok(Ok(o)) => o,
        _ => return vec![],
    };
    let text = String::from_utf8_lossy(&output.stdout);
    #[cfg(target_os = "windows")]
    return text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',');
            if !fields
                .next()?
                .trim_matches('"')
                .eq_ignore_ascii_case("accio.exe")
            {
                return None;
            }
            fields.next()?.trim_matches('"').parse().ok()
        })
        .collect();
    #[cfg(not(target_os = "windows"))]
    text.lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}
pub async fn inspect(cfg: &Value) -> Value {
    let paths = cfg["accio"]["customExePath"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| vec![PathBuf::from(s)])
        .unwrap_or_else(candidates);
    let root = home().join(".accio");
    let pids = running_pids().await;
    tokio::task::spawn_blocking(move || inspect_at(&paths, &root, &pids))
        .await
        .unwrap_or_else(|_| json!({"installed":false,"error":"inspection failed"}))
}
pub fn launch_plan(
    cfg: &Value,
    body: &Value,
    detection: &Value,
) -> Result<(PathBuf, String), (u16, String)> {
    if detection["installed"] != true {
        return Err((404, "Accio Work 客户端未安装".into()));
    }
    let force = body["force"] == true;
    if detection["edition"] != "cn" && !force && cfg["accio"]["allowNonCN"] != true {
        return Err((
            400,
            "needsForce: 非中国版或未识别版本，需 force: true 确认".into(),
        ));
    }
    if detection["running"] == true && !force {
        return Err((
            409,
            "needsForce: Accio 正在运行，请先退出或确认 force: true".into(),
        ));
    }
    let path = detection["path"]
        .as_str()
        .ok_or((404, "Accio executable missing".into()))?;
    let host = cfg["server"]["host"].as_str().unwrap_or("127.0.0.1");
    let host = if host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok((
        PathBuf::from(path),
        format!(
            "http://{}:{}",
            host,
            cfg["server"]["port"].as_u64().unwrap_or(8080)
        ),
    ))
}
pub async fn launch(cfg: &Value, body: &Value) -> Result<Value, (u16, String)> {
    let mut cfg = cfg.clone();
    if let Some(custom) = body["customExePath"].as_str() {
        if cfg["accio"].is_null() {
            cfg["accio"] = json!({});
        }
        cfg["accio"]["customExePath"] = json!(custom);
    }
    let detection = inspect(&cfg).await;
    let (path, gateway) = launch_plan(&cfg, body, &detection)?;
    let mut command = tokio::process::Command::new(&path);
    command
        .env_remove("ACCIO_BRIDGE_TOKEN")
        .env_remove("ACCIO_BRIDGE_CONFIG");
    command
        .env("GATEWAY_BASE_URL", &gateway)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|_| (500, "无法启动 Accio 客户端，请检查可执行文件和权限".into()))?;
    let pid = child.id();
    tokio::spawn(async move {
        let mut child = child;
        let _ = child.wait().await;
    });
    Ok(
        json!({"ok":true,"launched":true,"pid":pid,"gatewayBaseUrl":gateway,"edition":detection["edition"],"editionLabel":detection["editionLabel"],"path":path}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_validation_without_process() {
        let cfg = json!({"server":{"host":"127.0.0.1","port":8888}});
        let detected =
            json!({"installed":true,"path":"/fixture/Accio","edition":"unknown","running":false});
        assert_eq!(launch_plan(&cfg, &json!({}), &detected).unwrap_err().0, 400);
        assert_eq!(
            launch_plan(&cfg, &json!({"force":true}), &detected)
                .unwrap()
                .1,
            "http://127.0.0.1:8888"
        );
        assert_eq!(
            launch_plan(&cfg, &json!({}), &json!({"installed":false}))
                .unwrap_err()
                .0,
            404
        );
    }
    #[test]
    fn fixture_inspection() {
        let root = std::env::temp_dir().join(format!(
            "accio-inspection-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("resources")).unwrap();
        let exe = root.join("Accio");
        std::fs::write(&exe, "").unwrap();
        std::fs::write(
            root.join("domain-config.json"),
            r#"{"region":"CN","services":{"client":{"domain":"evil-accio.com"}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("appInstall.jsonl"),
            r#"{"packageRegion":"GLOBAL"}"#,
        )
        .unwrap();
        let result = inspect_at(&[exe], &root, &[]);
        assert_eq!(result["edition"], "cn");
        assert_eq!(result["scores"]["global"], 0);
        assert!(result["version"].is_null());
        assert!(result["gatewayUrl"].is_null());
        std::fs::remove_dir_all(root).unwrap();
    }
}
