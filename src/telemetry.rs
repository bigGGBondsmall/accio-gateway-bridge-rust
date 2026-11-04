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
    min_level: u8,
}
pub type Shared = Arc<Mutex<Telemetry>>;
/// Same depth as the Node gateway's ring buffer (`LOG_BUFFER_CAPACITY`).
pub const LOG_CAPACITY: usize = 1000;
/// Recording levels, quietest first. A line is stored only when its level is at or
/// above the threshold, so `debug` costs nothing until it is switched on.
pub const LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];
fn weight(level: &str) -> u8 {
    match level {
        "error" => 3,
        "warn" => 2,
        "info" => 1,
        _ => 0,
    }
}
impl Default for Telemetry {
    fn default() -> Self {
        Self {
            models: BTreeMap::new(),
            logs: VecDeque::new(),
            since: crate::discovery::timestamp(),
            min_level: 1,
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
            let level = if self.success { "info" } else { "error" };
            if weight(level) >= state.min_level {
                state.logs.push_back(json!({"ts":now(),"level":level,"msg":if self.success{"Generation completed"}else{"Generation failed or client disconnected"},"model":self.id}));
                while state.logs.len() > LOG_CAPACITY {
                    state.logs.pop_front();
                }
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
    let threshold = params.get("level").map(|s| weight(s.as_ref())).unwrap_or(0);
    let mut lines: Vec<_> = s
        .logs
        .iter()
        .filter(|v| weight(v["level"].as_str().unwrap_or("debug")) >= threshold)
        .rev()
        .take(count)
        .cloned()
        .collect();
    lines.reverse();
    json!({"ok":true,"lines":lines,"total":lines.len()})
}
/// Record one line in the bounded log ring, so bridge-side diagnostics (forwarded tool
/// sets, upstream error pages, capture) show up in the log view and not only on stderr.
/// Lines below the recording threshold are dropped, so a debug line has to be asked for.
pub fn log(state: &Shared, level: &str, message: &str) {
    let Ok(mut s) = state.lock() else {
        return;
    };
    if weight(level) < s.min_level {
        return;
    }
    s.logs
        .push_back(json!({"ts":now(),"level":level,"msg":message}));
    while s.logs.len() > LOG_CAPACITY {
        s.logs.pop_front();
    }
}
/// Current recording threshold, as a level name.
pub fn level(state: &Shared) -> String {
    let name = state
        .lock()
        .map(|s| LEVELS[(s.min_level as usize).min(LEVELS.len() - 1)])
        .unwrap_or("info");
    name.to_string()
}
/// Change the recording threshold: returns the canonical name, or why it was rejected.
pub fn set_level(state: &Shared, level: &str) -> Result<String, String> {
    let wanted = level.trim().to_ascii_lowercase();
    let index = LEVELS
        .iter()
        .position(|name| *name == wanted)
        .ok_or_else(|| {
            format!(
                "unknown log level {:?}; use one of {}",
                level,
                LEVELS.join(", ")
            )
        })?;
    let mut s = state
        .lock()
        .map_err(|_| "telemetry lock poisoned".to_string())?;
    s.min_level = index as u8;
    Ok(LEVELS[index].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_threshold_hides_debug_until_it_is_switched_on() {
        let shared: Shared = Default::default();
        assert_eq!(level(&shared), "info");
        log(&shared, "debug", "hidden");
        assert_eq!(logs(&shared, "")["total"].as_u64(), Some(0));
        assert_eq!(set_level(&shared, "DEBUG").unwrap(), "debug");
        log(&shared, "debug", "shown");
        let recorded = logs(&shared, "");
        assert_eq!(recorded["total"].as_u64(), Some(1));
        assert_eq!(recorded["lines"][0]["msg"].as_str(), Some("shown"));
        assert_eq!(set_level(&shared, "warn").unwrap(), "warn");
        log(&shared, "info", "below the threshold");
        assert_eq!(logs(&shared, "")["total"].as_u64(), Some(1));
        assert!(set_level(&shared, "")
            .unwrap_err()
            .contains("unknown log level"));
        assert!(set_level(&shared, "verbose")
            .unwrap_err()
            .contains("unknown log level"));
        assert_eq!(level(&shared), "warn");
    }

    #[test]
    fn reading_filter_still_narrows_a_debug_ring() {
        let shared: Shared = Default::default();
        set_level(&shared, "debug").unwrap();
        log(&shared, "debug", "d");
        log(&shared, "info", "i");
        log(&shared, "error", "e");
        assert_eq!(logs(&shared, "")["total"].as_u64(), Some(3));
        assert_eq!(logs(&shared, "level=info")["total"].as_u64(), Some(2));
        let errors = logs(&shared, "level=error");
        assert_eq!(errors["lines"].as_array().unwrap().len(), 1);
        assert_eq!(errors["lines"][0]["msg"].as_str(), Some("e"));
    }
}
