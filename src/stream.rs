use crate::protocol;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Byte framing avoids corrupting UTF-8 split across network reads.
pub struct Decoder {
    buffer: Vec<u8>,
    data: Vec<String>,
    data_bytes: usize,
    ollama: bool,
    skip_lf: bool,
}
impl Decoder {
    pub fn new(ollama: bool) -> Self {
        Self {
            buffer: Vec::new(),
            data: Vec::new(),
            data_bytes: 0,
            ollama,
            skip_lf: false,
        }
    }
    fn line(&mut self) -> Result<Option<String>, String> {
        let raw = std::mem::take(&mut self.buffer);
        let line = String::from_utf8(raw).map_err(|_| "invalid upstream UTF-8")?;
        if self.ollama {
            return Ok((!line.trim().is_empty()).then_some(line));
        }
        if line.is_empty() {
            if self.data.is_empty() {
                return Ok(None);
            }
            self.data_bytes = 0;
            return Ok(Some(std::mem::take(&mut self.data).join("\n")));
        }
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            // Count separators as well as payload, and bound Vec overhead for empty lines.
            self.data_bytes = self
                .data_bytes
                .checked_add(data.len())
                .and_then(|bytes| bytes.checked_add(usize::from(!self.data.is_empty())))
                .ok_or("upstream event exceeds limit")?;
            if self.data_bytes > 1024 * 1024 || self.data.len() >= 16_384 {
                return Err("upstream event exceeds limit".into());
            }
            self.data.push(data.to_string());
        }
        Ok(None)
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        let mut events = vec![];
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\n' || byte == b'\r' {
                self.skip_lf = byte == b'\r';
                if let Some(event) = self.line()? {
                    events.push(event);
                }
            } else {
                self.buffer.push(byte);
                if self.buffer.len() > 1024 * 1024 {
                    return Err("upstream frame exceeds 1 MiB".into());
                }
            }
        }
        Ok(events)
    }
    pub fn eof(&mut self) -> Result<Vec<String>, String> {
        let mut events = vec![];
        if !self.buffer.is_empty() {
            if let Some(e) = self.line()? {
                events.push(e);
            }
        }
        if !self.ollama && !self.data.is_empty() {
            self.data_bytes = 0;
            events.push(std::mem::take(&mut self.data).join("\n"));
        }
        Ok(events)
    }
}
pub struct Sink {
    calls: BTreeMap<u64, Value>,
    index_keys: BTreeMap<Option<u64>, u64>,
    next_call: u64,
    thoughts_enabled: bool,
    emitted_parts: bool,
    usage: Option<Value>,
    reason: Option<String>,
    pub terminal: bool,
    finished: bool,
}
impl Sink {
    pub fn new(thoughts_enabled: bool) -> Self {
        Self {
            index_keys: BTreeMap::new(),
            next_call: 0,
            thoughts_enabled,
            emitted_parts: false,
            calls: BTreeMap::new(),
            usage: None,
            reason: None,
            terminal: false,
            finished: false,
        }
    }
    pub fn event(&mut self, s: &str, ollama: bool) -> Result<Vec<Value>, String> {
        if s.trim() == "[DONE]" {
            self.terminal = true;
            return Ok(vec![]);
        }
        let v: Value = serde_json::from_str(s).map_err(|_| "malformed upstream JSON event")?;
        if v.get("error").is_some() {
            return Err("upstream reported an error".into());
        }
        let mut out = vec![];
        if ollama {
            if let Some(t) = v["message"]["content"].as_str() {
                if !t.is_empty() {
                    out.push(protocol::text_frame(t));
                }
            }
            if self.thoughts_enabled {
                if let Some(t) = v["message"]["thinking"]
                    .as_str()
                    .or_else(|| v["thinking"].as_str())
                    .filter(|t| !t.is_empty())
                {
                    out.push(protocol::thought_frame(t));
                }
            }
            if let Some(calls) = v["message"]["tool_calls"].as_array() {
                for c in calls {
                    let f = &c["function"];
                    self.calls.insert(
                        self.next_call,
                        json!({"id":c["id"],"name":f["name"],"args":f["arguments"]}),
                    );
                    self.next_call += 1;
                }
            }
            if v["done"] == true {
                self.terminal = true;
                self.finished = true;
                self.reason = Some(
                    if v["done_reason"] == "length" {
                        "length"
                    } else {
                        "stop"
                    }
                    .into(),
                );
                self.usage = Some(
                    json!({"prompt_tokens":v["prompt_eval_count"],"completion_tokens":v["eval_count"]}),
                );
            }
        } else {
            if v["usage"].is_object() {
                self.usage = Some(v["usage"].clone());
            }
            if let Some(choices) = v["choices"].as_array() {
                for c in choices
                    .iter()
                    .filter(|c| c["index"].as_u64().unwrap_or(0) == 0)
                {
                    let d = &c["delta"];
                    if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
                        out.push(protocol::text_frame(t));
                    }
                    if self.thoughts_enabled {
                        if let Some(t) = d["reasoning_content"]
                            .as_str()
                            .filter(|t| !t.is_empty())
                            .or_else(|| d["reasoning"].as_str().filter(|t| !t.is_empty()))
                        {
                            out.push(protocol::thought_frame(t));
                        }
                    }
                    if let Some(cs) = d["tool_calls"].as_array() {
                        for call in cs {
                            let slot = call["index"].as_u64();
                            let id = call["id"].as_str().filter(|id| !id.is_empty());
                            let existing = self.index_keys.get(&slot).copied().filter(|key| {
                                let old = self.calls[key]["id"].as_str();
                                !matches!((old, id), (Some(a), Some(b)) if a != b)
                            });
                            let key = existing.unwrap_or_else(|| {
                                let key = self.next_call;
                                self.next_call += 1;
                                self.index_keys.insert(slot, key);
                                key
                            });
                            let entry = self
                                .calls
                                .entry(key)
                                .or_insert_with(|| json!({"name":"","args":""}));
                            if let Some(id) = id {
                                entry["id"] = json!(id);
                            }
                            if entry["name"].as_str().unwrap_or("").is_empty() {
                                if let Some(name) = call["function"]["name"].as_str() {
                                    entry["name"] = json!(name);
                                }
                            }
                            if let Some(args) = call["function"]["arguments"].as_str() {
                                let mut text = entry["args"].as_str().unwrap_or("").to_string();
                                text.push_str(args);
                                if text.len() > 1024 * 1024 {
                                    return Err("tool arguments exceed 1 MiB".into());
                                }
                                entry["args"] = json!(text);
                            }
                        }
                    }
                    if let Some(r) = c["finish_reason"].as_str() {
                        self.reason = Some(r.to_string());
                        self.finished = true;
                    }
                }
            }
        }
        if self.calls.len() > 128
            || self
                .calls
                .values()
                .map(|v| {
                    v["args"]
                        .as_str()
                        .map(str::len)
                        .unwrap_or_else(|| v["args"].to_string().len())
                })
                .sum::<usize>()
                > 1024 * 1024
        {
            return Err("tool call accumulation exceeds limit".into());
        }
        self.emitted_parts |= !out.is_empty();
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<Value, String> {
        if !self.terminal && !self.finished {
            return Err("upstream stream ended before completion".into());
        }
        if self.calls.is_empty()
            && !self.emitted_parts
            && self.usage.is_none()
            && self.reason.is_none()
        {
            return Err("upstream returned an empty response stream".into());
        }
        if self.calls.is_empty() {
            Ok(protocol::done_frame(
                self.usage.take(),
                self.reason.as_deref(),
            ))
        } else {
            // A call the upstream never named is not a call. The previous build (and the Node
            // implementation) drop those entries, which also lets the empty-stream check above
            // report a stream that carried nothing usable instead of delivering `name: ""`.
            let calls: Vec<Value> = std::mem::take(&mut self.calls)
                .into_values()
                .filter(|call| !call["name"].as_str().unwrap_or("").is_empty())
                .collect();
            if calls.is_empty() {
                return Err("upstream returned an empty response stream".into());
            }
            // A call the upstream never finished writing is reported as a failure instead of being
            // handed over as a runnable tool call; the usage it already reported is kept.
            if let Some(problem) = protocol::tool_calls_incomplete(self.reason.as_deref()) {
                let mut frame = error_frame(&problem);
                if let Some(tokens) = self.usage.take() {
                    if tokens.is_object() {
                        frame["usageMetadata"] = protocol::usage(&tokens);
                    }
                }
                return Ok(frame);
            }
            Ok(protocol::calls_frame(calls, self.usage.take()))
        }
    }
}
pub fn error_frame(message: &str) -> Value {
    json!({"error":{"code":502,"message":message,"status":"UPSTREAM_ERROR"},"turnComplete":true})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_utf8_and_multiline() {
        let mut d = Decoder::new(false);
        assert!(d.feed(b"data: \xe4").unwrap().is_empty());
        assert!(d.feed(b"\xbd\xa0\r\n").unwrap().is_empty());
        assert_eq!(d.feed(b"data: two\r\n\r\n").unwrap(), vec!["你\ntwo"]);
    }
    #[test]
    fn eof_is_not_success() {
        assert!(Sink::new(true).finish().is_err());
    }
    #[test]
    fn reasoning_is_separate_and_can_be_disabled() {
        let event =
            r#"{"choices":[{"delta":{"reasoning_content":"private thought","content":"answer"}}]}"#;
        let frames = Sink::new(true).event(event, false).unwrap();
        assert_eq!(frames[0]["content"]["parts"][0]["text"], "answer");
        assert_eq!(frames[1]["content"]["parts"][0]["thought"], true);
        let frames = Sink::new(false).event(event, false).unwrap();
        assert_eq!(frames.len(), 1);
        let alias = r#"{"choices":[{"delta":{"reasoning":"thought alias"}}]}"#;
        assert_eq!(
            Sink::new(true).event(alias, false).unwrap()[0]["content"]["parts"][0]["thought"],
            true
        );
    }
    #[test]
    fn ollama_thinking_and_length_are_preserved() {
        let event = r#"{"message":{"thinking":"hmm","content":"answer"},"done":true,"done_reason":"length"}"#;
        let mut sink = Sink::new(true);
        let frames = sink.event(event, true).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1]["content"]["parts"][0]["thought"], true);
        assert_eq!(sink.finish().unwrap()["finishReason"], "MAX_TOKENS");
        assert_eq!(Sink::new(false).event(event, true).unwrap().len(), 1);
    }
    #[test]
    fn reused_indexes_keep_distinct_calls_and_argument_fragments() {
        let mut sink = Sink::new(true);
        for (id, args) in [
            (Some("first"), "{\"x\":"),
            (None, "1}"),
            (Some("second"), "{\"y\":2}"),
        ] {
            let mut call = json!({"index":0,"function":{"arguments":args}});
            if let Some(id) = id {
                call["id"] = json!(id);
                call["function"]["name"] = json!("read");
            }
            sink.event(
                &json!({"choices":[{"delta":{"tool_calls":[call]}}]}).to_string(),
                false,
            )
            .unwrap();
        }
        sink.event("[DONE]", false).unwrap();
        let result = sink.finish().unwrap();
        let parts = result["content"]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["functionCall"]["id"], "first");
        assert_eq!(parts[0]["functionCall"]["args"]["x"], 1);
        assert_eq!(parts[1]["functionCall"]["id"], "second");
        assert_eq!(parts[1]["functionCall"]["args"]["y"], 2);
    }
    #[test]
    fn done_only_is_not_a_blank_success() {
        let mut sink = Sink::new(true);
        sink.event("[DONE]", false).unwrap();
        assert!(sink.finish().unwrap_err().contains("empty response"));
    }
    #[test]
    fn empty_sse_data_lines_are_bounded() {
        let mut decoder = Decoder::new(false);
        assert!(decoder
            .feed(b"data:\n".repeat(16_384).as_slice())
            .unwrap()
            .is_empty());
        assert!(decoder
            .feed(b"data:\n")
            .unwrap_err()
            .contains("exceeds limit"));
    }
    #[test]
    fn sse_event_byte_accounting_resets_between_events() {
        let mut decoder = Decoder::new(false);
        let payload = format!("data: {}\n\n", "a".repeat(600_000));
        for _ in 0..3 {
            assert_eq!(decoder.feed(payload.as_bytes()).unwrap()[0].len(), 600_000);
        }
        assert!(decoder
            .feed(format!("data: {}\n", "a".repeat(600_000)).as_bytes())
            .unwrap()
            .is_empty());
        assert!(decoder
            .feed(format!("data: {}\n", "b".repeat(600_000)).as_bytes())
            .is_err());
    }
    #[test]
    fn ndjson_tail() {
        let mut d = Decoder::new(true);
        d.feed(b"{\"done\":true}").unwrap();
        assert_eq!(d.eof().unwrap().len(), 1);
    }
    #[test]
    fn truncated_tool_call_is_reported_not_delivered() {
        // Upstream streams a tool call, then stops because it hit the length limit.
        let mut sink = Sink::new(true);
        sink.event(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{\"path\":\"a.tx"}}]}}]}).to_string(),
            false,
        )
        .unwrap();
        sink.event(
            &json!({"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":5,"completion_tokens":9}}).to_string(),
            false,
        )
        .unwrap();
        let frame = sink.finish().unwrap();
        assert!(
            frame["error"]["message"]
                .as_str()
                .unwrap()
                .contains("length"),
            "{frame}"
        );
        assert_eq!(frame["turnComplete"], true);
        assert!(
            frame["content"].is_null(),
            "an unfinished call must not be delivered"
        );
        assert_eq!(frame["usageMetadata"]["totalTokenCount"], json!(14));
    }

    #[test]
    fn finished_tool_call_keeps_usage_and_reports_tool_calls() {
        let mut sink = Sink::new(true);
        sink.event(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}}]}}]}).to_string(),
            false,
        )
        .unwrap();
        sink.event(
            &json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":9}}).to_string(),
            false,
        )
        .unwrap();
        let frame = sink.finish().unwrap();
        assert_eq!(frame["finishReason"], "TOOL_CALLS");
        assert_eq!(frame["usageMetadata"]["totalTokenCount"], json!(14));
        assert_eq!(frame["content"]["parts"][0]["functionCall"]["name"], "read");
    }
    #[test]
    fn a_nameless_tool_call_is_not_a_call() {
        // Upstream announces a call but never names it; the older build (and the Node port) drop
        // such entries, which lets the empty-stream check report that nothing usable arrived.
        let mut sink = Sink::new(true);
        sink.event(
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"arguments":"{}"}}]}}]}).to_string(),
            false,
        )
        .unwrap();
        sink.event("[DONE]", false).unwrap();
        let error = sink.finish().unwrap_err();
        assert!(error.contains("empty response"), "{error}");
    }
}
