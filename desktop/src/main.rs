#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::Manager;
use tauri_plugin_shell::{
    process::{CommandChild, CommandEvent},
    ShellExt,
};

struct Session {
    lifecycle: Mutex<Lifecycle>,
    token: String,
    child: Mutex<Option<CommandChild>>,
    client: reqwest::Client,
}
// Take ownership before stopping: cleanup remains effective even while the reader task
// retains another Arc, and repeated Exit/error cleanup cannot kill twice.
fn stop_child<T>(slot: &Mutex<Option<T>>, stop: impl FnOnce(T)) {
    let child = slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(child) = child {
        stop(child);
    }
}
impl Session {
    fn stop(&self) {
        self.lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .fail();
        stop_child(&self.child, |child| {
            let _ = child.kill();
        });
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        // Last-owner fallback only; startup errors and Exit explicitly invoke stop().
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(child) = child {
            let _ = child.kill();
        }
    }
}
mod lifecycle;
use lifecycle::{allowed, Lifecycle};
fn management_target(endpoint: &str, path: &str) -> Result<reqwest::Url, String> {
    let target = reqwest::Url::parse(&format!("{endpoint}{path}")).map_err(|_| "管理路径无效")?;
    let (expected_path, expected_query) = path
        .split_once('?')
        .map(|(p, q)| (p, Some(q)))
        .unwrap_or((path, None));
    if target.path() != expected_path
        || target.query() != expected_query
        || target.fragment().is_some()
    {
        return Err("管理路径不可规范化或重定向".into());
    }
    Ok(target)
}
#[tauri::command]
async fn management_request(
    state: tauri::State<'_, Arc<Session>>,
    path: String,
    method: String,
    body: Option<Value>,
) -> Result<Value, String> {
    if !allowed(&method, &path) {
        return Err("不允许的管理操作".into());
    }
    if body
        .as_ref()
        .is_some_and(|v| v.to_string().len() > 1_048_576)
    {
        return Err("配置过大".into());
    }
    let mut endpoint = None;
    for _ in 0..100 {
        if matches!(
            *state.lifecycle.lock().map_err(|_| "状态不可用")?,
            Lifecycle::Failed
        ) {
            return Err("本地网关启动失败，请检查打包的网关程序和配置文件".into());
        }
        endpoint = state.lifecycle.lock().map_err(|_| "状态不可用")?.endpoint();
        if endpoint.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let endpoint = endpoint.ok_or("本地网关尚未启动，请稍后重试")?;
    let target = management_target(&endpoint, &path)?;
    let mut request = state
        .client
        .request(method.parse().map_err(|_| "操作无效")?, target)
        .header("X-Gateway-Token", &state.token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|_| "本地网关连接失败")?;
    if !response.status().is_success() {
        return Err(format!(
            "管理请求失败（HTTP {}），请检查配置和权限",
            response.status().as_u16()
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "响应读取失败")?;
        if bytes.len() + chunk.len() > 4_194_304 {
            return Err("响应过大".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "管理响应格式无效".into())
}
fn main() {
    let state = Arc::new(Session {
        lifecycle: Mutex::new(Lifecycle::Starting),
        token: uuid::Uuid::new_v4().to_string(),
        child: Mutex::new(None),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(12))
            .build()
            .expect("HTTP client"),
    });
    let session = state.clone();
    let cleanup = state.clone();
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![management_request])
        .setup(move |app| {
            let dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&dir)?;
            let config = dir.join("config.json");
            let result = app
                .shell()
                .sidecar("accio-gateway-bridge-rust")
                .and_then(|cmd| {
                    cmd.args([
                        "--config",
                        &config.to_string_lossy(),
                        "--host",
                        "127.0.0.1",
                        "--port",
                        "0",
                    ])
                    .env("ACCIO_BRIDGE_TOKEN", &session.token)
                    .spawn()
                });
            match result {
                Ok((mut rx, child)) => {
                    *session.child.lock().unwrap() = Some(child);
                    let live = session.clone();
                    tauri::async_runtime::spawn(async move {
                        while let Some(event) = rx.recv().await {
                            match event {
                                CommandEvent::Stdout(line) => {
                                    if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                                        if value["event"] == "listening"
                                            && value["host"] == "127.0.0.1"
                                        {
                                            if let Some(port) = value["port"]
                                                .as_u64()
                                                .filter(|p| *p > 0 && *p <= 65535)
                                            {
                                                live.lifecycle
                                                    .lock()
                                                    .unwrap()
                                                    .ready("127.0.0.1", port);
                                            }
                                        }
                                    }
                                }
                                CommandEvent::Terminated(_) | CommandEvent::Error(_) => {
                                    live.lifecycle.lock().unwrap().fail();
                                }
                                _ => {} // Deliberately never relay sidecar stderr, configuration, or secrets to the webview.
                            }
                        }
                    });
                }
                Err(_) => {
                    session.lifecycle.lock().unwrap().fail();
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!());
    let app = match result {
        Ok(app) => app,
        Err(_) => {
            // Do not rely on Drop: the stdout task may still own Arc<Session>.
            cleanup.stop();
            eprintln!("Desktop startup failed; local gateway cleanup requested.");
            return;
        }
    };
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            let session = handle.state::<Arc<Session>>();
            session.stop();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_cleanup_works_with_reader_arc_and_is_idempotent() {
        let slot = Arc::new(Mutex::new(Some(7u8)));
        let reader = slot.clone();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        stop_child(&slot, |child| {
            assert_eq!(child, 7);
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(reader.lock().unwrap().is_none());
        stop_child(&slot, |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[test]
    fn normalized_urls_never_leave_proxy() {
        for path in [
            "/api/bridge/models/..\\config",
            "/api/bridge/models/%2e%2e/config",
            "/api/bridge/models/../config",
            "/api/bridge/models/a#fragment",
        ] {
            assert!(management_target("http://127.0.0.1:8080", path).is_err());
        }
    }
    #[test]
    fn encoded_model_ids_and_delete_query_are_preserved() {
        for path in [
            "/api/bridge/models/org%2Fmodel",
            "/api/bridge/models/name%3Fpart%23fragment",
            "/api/bridge/models?id=org%2Fmodel",
        ] {
            let url = management_target("http://127.0.0.1:8080", path).unwrap();
            assert_eq!(url.as_str(), format!("http://127.0.0.1:8080{path}"));
        }
    }
}
