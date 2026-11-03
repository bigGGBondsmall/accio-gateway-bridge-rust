use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};
pub struct Telemetry {
    models: BTreeMap<String, Value>,
    logs: VecDeque<Value>,
    since: String,
}
pub type Shared = Arc<Mutex<Telemetry>>;
/// Same depth as the Node gateway's ring buffer (`LOG_BUFFER_CAPACITY`).
pub const LOG_CAPACITY: usize = 1000;
impl Default for Telemetry {
    fn default() -> Self {
        Self {
            models: BTreeMap::new(),
            logs: VecDeque::new(),
            since: crate::discovery::timestamp(),
        }
    }
}
fn now() -> String {
    crate::discovery::timestamp()
}
pub struct Turn {
    state: Shared,
    id: String,
    start: Instant,
    success: bool,
}
impl Turn {
    pub fn new(state: Shared, id: String) -> Self {
        if let Ok(mut s) = state.lock() {
            let entry = s.models.entry(id.clone()).or_insert_with(
                || json!({"requests":0,"errors":0,"lastLatencyMs":0,"lastUsedAt":0}),
            );
            entry["requests"] = json!(entry["requests"].as_u64().unwrap_or(0) + 1);
        }
        Self {
            state,
            id,
            start: Instant::now(),
            success: false,
        }
    }
    pub fn success(&mut self) {
        self.success = true;
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(entry) = state.models.get_mut(&self.id) {
                if !self.success {
                    entry["errors"] = json!(entry["errors"].as_u64().unwrap_or(0) + 1);
                }
                entry["lastLatencyMs"] = json!(self.start.elapsed().as_millis() as u64);
                entry["lastUsedAt"] = json!(now());
            }
            state.logs.push_back(json!({"ts":now(),"level":if self.success{"info"}else{"error"},"msg":if self.success{"Generation completed"}else{"Generation failed or client disconnected"},"model":self.id}));
            while state.logs.len() > LOG_CAPACITY {
                state.logs.pop_front();
            }
        }
    }
}
pub fn usage(state: &Shared) -> Value {
    let s = state.lock().unwrap();
    json!({"ok":true,"usage":s.models,"since":s.since})
}
pub fn logs(state: &Shared, query: &str) -> Value {
    let s = state.lock().unwrap();
    let params: std::collections::HashMap<_, _> =
        url::form_urlencoded::parse(query.as_bytes()).collect();
    let count = params
        .get("lines")
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200)
        .min(LOG_CAPACITY);
    let threshold = match params.get("level").map(|s| s.as_ref()) {
        Some("error") => 3,
        Some("warn") => 2,
        Some("info") => 1,
        _ => 0,
    };
    let mut lines:Vec<_>=s.logs.iter().filter(|v|match v["level"].as_str(){Some("error")=>3,Some("warn")=>2,Some("info")=>1,_=>0}>=threshold).rev().take(count).cloned().collect();
    lines.reverse();
    json!({"ok":true,"lines":lines,"total":lines.len()})
}
/// Record one line in the bounded log ring, so bridge-side diagnostics (forwarded tool
/// sets, upstream error pages, capture) show up in the log view and not only on stderr.
pub fn log(state: &Shared, level: &str, message: &str) {
    if let Ok(mut s) = state.lock() {
        s.logs
            .push_back(json!({"ts":now(),"level":level,"msg":message}));
        while s.logs.len() > LOG_CAPACITY {
            s.logs.pop_front();
        }
    }
}
