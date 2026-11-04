use crate::protocol;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Write;

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
/// Magic that starts a gzip member; the buffered path sniffs it too when the header is missing.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// One compressed chunk cannot be allowed to expand without bound: a decompression bomb would reach
/// memory before the frame splitter ever saw a line break (and it rejects frames at 1 MiB anyway).
const MAX_DECODED_CHUNK: usize = 8 * 1024 * 1024;
const INFLATE_FAILED: &str = "invalid compressed upstream stream";
/// The framing of a stream cannot be replayed, so its first bytes have to decide it.
#[derive(Clone, Copy)]
enum Framing {
    /// No `content-encoding`: identity, unless the first bytes turn out to be a gzip member.
    Unknown,
    /// `content-encoding: deflate`: two bytes tell a zlib wrapper from raw DEFLATE.
    Deflate,
    /// The codec below is final.
    Decided,
}
enum Codec {
    Identity,
    Gzip(Box<flate2::write::GzDecoder<Vec<u8>>>),
    Zlib(Box<flate2::write::ZlibDecoder<Vec<u8>>>),
    Deflate(Box<flate2::write::DeflateDecoder<Vec<u8>>>),
    Brotli(Box<brotli::DecompressorWriter<Vec<u8>>>),
}
impl Codec {
    /// Decoded output is handed to the inner buffer by the codec's own `dump` step, which only runs
    /// on the next write or on `flush`: without the flush half of a decompressed chunk would stay
    /// inside the decoder and the frames built from it would never be seen.
    fn inflate(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Codec::Identity => Ok(bytes.to_vec()),
            Codec::Gzip(decoder) => {
                decoder.write_all(bytes).map_err(|_| INFLATE_FAILED)?;
                decoder.flush().map_err(|_| INFLATE_FAILED)?;
                Ok(std::mem::take(decoder.get_mut()))
            }
            Codec::Zlib(decoder) => {
                decoder.write_all(bytes).map_err(|_| INFLATE_FAILED)?;
                decoder.flush().map_err(|_| INFLATE_FAILED)?;
                Ok(std::mem::take(decoder.get_mut()))
            }
            Codec::Deflate(decoder) => {
                decoder.write_all(bytes).map_err(|_| INFLATE_FAILED)?;
                decoder.flush().map_err(|_| INFLATE_FAILED)?;
                Ok(std::mem::take(decoder.get_mut()))
            }
            Codec::Brotli(decoder) => {
                decoder.write_all(bytes).map_err(|_| INFLATE_FAILED)?;
                Ok(std::mem::take(decoder.get_mut()))
            }
        }
    }
    /// Release whatever the codec is still holding: brotli keeps a tail until it is closed, the
    /// flate2 writers only have the buffer `dump` has not written yet.
    fn finish(&mut self) -> Vec<u8> {
        match self {
            Codec::Identity => Vec::new(),
            Codec::Brotli(decoder) => {
                // A truncated stream is reported by the frame decoder's own end-of-stream check,
                // which is the error the caller already understands.
                let _ = decoder.close();
                std::mem::take(decoder.get_mut())
            }
            Codec::Gzip(decoder) => {
                let _ = decoder.flush();
                std::mem::take(decoder.get_mut())
            }
            Codec::Zlib(decoder) => {
                let _ = decoder.flush();
                std::mem::take(decoder.get_mut())
            }
            Codec::Deflate(decoder) => {
                let _ = decoder.flush();
                std::mem::take(decoder.get_mut())
            }
        }
    }
}
/// Decode an upstream body stream according to its `content-encoding`.
///
/// A custom provider that compresses unconditionally used to hand its compressed bytes straight to
/// [`Decoder`], which turned a perfectly good answer into "invalid upstream UTF-8": the streaming
/// path is consumed chunk by chunk, so the buffered helper in `main.rs` cannot be reused here.
pub struct BodyDecoder {
    framing: Framing,
    codec: Codec,
    /// The first bytes of an undecided stream, held until the framing decision is possible.
    pending: Vec<u8>,
}
impl BodyDecoder {
    /// `encoding` is the lowercased `content-encoding` header value. An empty value still sniffs a
    /// gzip magic, exactly like the buffered path does, because a provider may compress without
    /// saying so.
    pub fn new(encoding: &str) -> Result<Self, String> {
        let (framing, codec) = match encoding {
            "" | "identity" => (Framing::Unknown, Codec::Identity),
            "gzip" => (
                Framing::Decided,
                Codec::Gzip(Box::new(flate2::write::GzDecoder::new(Vec::new()))),
            ),
            "deflate" => (
                Framing::Deflate,
                Codec::Zlib(Box::new(flate2::write::ZlibDecoder::new(Vec::new()))),
            ),
            "br" => (
                Framing::Decided,
                Codec::Brotli(Box::new(brotli::DecompressorWriter::new(Vec::new(), 4096))),
            ),
            _ => return Err("unsupported content encoding".into()),
        };
        Ok(Self {
            framing,
            codec,
            pending: Vec::new(),
        })
    }
    /// Decode one upstream chunk. The identity path stays copy-free, which is the common case.
    pub fn write<'a>(&mut self, bytes: &'a [u8]) -> Result<Cow<'a, [u8]>, String> {
        if bytes.is_empty() {
            return Ok(Cow::Borrowed(bytes));
        }
        match self.framing {
            Framing::Decided => match self.codec {
                Codec::Identity => Ok(Cow::Borrowed(bytes)),
                _ => Ok(Cow::Owned(self.decode(bytes)?)),
            },
            Framing::Unknown => {
                if self.pending.is_empty() && bytes[0] != GZIP_MAGIC[0] {
                    self.framing = Framing::Decided;
                    return Ok(Cow::Borrowed(bytes));
                }
                self.pending.extend_from_slice(bytes);
                if self.pending.len() < GZIP_MAGIC.len() {
                    return Ok(Cow::Owned(Vec::new()));
                }
                let held = std::mem::take(&mut self.pending);
                if held.starts_with(&GZIP_MAGIC) {
                    self.codec = Codec::Gzip(Box::new(flate2::write::GzDecoder::new(Vec::new())));
                }
                self.framing = Framing::Decided;
                Ok(Cow::Owned(self.decode(&held)?))
            }
            Framing::Deflate => {
                self.pending.extend_from_slice(bytes);
                if self.pending.len() < 2 {
                    return Ok(Cow::Owned(Vec::new()));
                }
                let head = u16::from_be_bytes([self.pending[0], self.pending[1]]);
                // A zlib header is `(method=8, window bits)` plus a check value: the first two
                // bytes are a multiple of 31. Anything else is raw DEFLATE.
                let zlib_wrapped = self.pending[0] & 0x0f == 8 && head.is_multiple_of(31);
                let held = std::mem::take(&mut self.pending);
                if !zlib_wrapped {
                    self.codec =
                        Codec::Deflate(Box::new(flate2::write::DeflateDecoder::new(Vec::new())));
                }
                self.framing = Framing::Decided;
                Ok(Cow::Owned(self.decode(&held)?))
            }
        }
    }
    fn decode(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let decoded = self.codec.inflate(bytes)?;
        if decoded.len() > MAX_DECODED_CHUNK {
            return Err("decoded stream chunk too large".into());
        }
        Ok(decoded)
    }
    /// Bytes the codec still holds; call once at end of stream, before the frame decoder's own
    /// end-of-stream handling.
    pub fn finish(&mut self) -> Vec<u8> {
        self.codec.finish()
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
    pub fn event(&mut self, s: &str, ollama: bool) -> Result<Vec<Value>, UpstreamError> {
        if s.trim() == "[DONE]" {
            self.terminal = true;
            return Ok(vec![]);
        }
        let v: Value = serde_json::from_str(s)
            .map_err(|_| UpstreamError::stream("malformed upstream JSON event"))?;
        // The provider's own error payload is kept: "upstream reported an error" alone told the
        // caller neither what happened nor whether retrying could help.
        if let Some(error) = v.get("error").filter(|error| !error.is_null()) {
            return Err(UpstreamError::provider(error));
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
    error_frame_with(502, message)
}
/// A failure frame that carries the code the failure is reported under: the HTTP status when the
/// upstream answered one, the provider's own numeric `code` when it supplied a usable one.
///
/// Transport and stream failures keep the 502 that [`error_frame`] has always used.
pub fn error_frame_with(code: u16, message: &str) -> Value {
    json!({"error":{"code":code,"message":message,"status":"UPSTREAM_ERROR"},"turnComplete":true})
}
/// An upstream failure and the frame code it should be reported under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamError {
    pub code: u16,
    pub message: String,
}
impl UpstreamError {
    /// A transport or stream failure: no provider answer was reached, so 502 stays.
    pub fn stream(message: impl Into<String>) -> Self {
        Self {
            code: 502,
            message: message.into(),
        }
    }
    /// A provider-reported failure, described from the provider's own error payload.
    ///
    /// Google reports its HTTP status in `error.code`, OpenAI reports a symbolic string there; the
    /// numeric form is a usable frame code and the symbolic one belongs in the message.
    pub fn provider(error: &Value) -> Self {
        let described = crate::ProviderError::from_value(error)
            .map(|error| error.describe())
            .unwrap_or_default();
        let message = if described.is_empty() {
            "upstream reported an error".to_string()
        } else {
            format!("upstream reported an error: {described}")
        };
        let code = error["code"]
            .as_u64()
            .filter(|code| (400..=599).contains(code))
            .map(|code| code as u16)
            .unwrap_or(502);
        Self { code, message }
    }
}
impl From<String> for UpstreamError {
    fn from(message: String) -> Self {
        Self::stream(message)
    }
}
impl From<&str> for UpstreamError {
    fn from(message: &str) -> Self {
        Self::stream(message)
    }
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
    /// Feed encoded chunks through the decoding layer exactly the way `generate()` does, and
    /// collect the frames the sink produced.
    fn frames_through(encoding: &str, chunks: &[Vec<u8>]) -> Vec<Value> {
        let mut body = BodyDecoder::new(encoding).unwrap();
        let mut decoder = Decoder::new(false);
        let mut sink = Sink::new(true);
        let mut frames = Vec::new();
        for chunk in chunks {
            let decoded = body.write(chunk).unwrap();
            for event in decoder.feed(&decoded).unwrap() {
                frames.extend(sink.event(&event, false).unwrap());
            }
        }
        let tail = body.finish();
        for event in decoder.feed(&tail).unwrap() {
            frames.extend(sink.event(&event, false).unwrap());
        }
        for event in decoder.eof().unwrap() {
            frames.extend(sink.event(&event, false).unwrap());
        }
        frames.push(sink.finish().unwrap());
        frames
    }
    #[test]
    fn gzip_compressed_sse_streams_through_the_decoder() {
        let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n\ndata: [DONE]\n\n";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload.as_bytes()).unwrap();
        let gzipped = encoder.finish().unwrap();
        // Seven-byte chunks cut inside the multi-byte character and inside the JSON, so the
        // decoder has to reassemble both.
        let chunks: Vec<Vec<u8>> = gzipped.chunks(7).map(<[u8]>::to_vec).collect();
        for encoding in ["gzip", ""] {
            // An empty header has to work as well: a provider may compress without saying so.
            let frames = frames_through(encoding, &chunks);
            assert_eq!(frames[0]["content"]["parts"][0]["text"], "你好");
            assert_eq!(frames[1]["content"]["parts"][0]["text"], "world");
            assert_eq!(frames[2]["turnComplete"], true);
            assert!(frames.iter().all(|frame| frame.get("error").is_none()));
        }
    }
    #[test]
    fn deflate_and_brotli_sse_are_decoded() {
        let payload =
            "data: {\"choices\":[{\"delta\":{\"content\":\"zlib\"}}]}\n\ndata: [DONE]\n\n";
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload.as_bytes()).unwrap();
        let zlib = encoder.finish().unwrap();
        let frames = frames_through("deflate", &[zlib[..3].to_vec(), zlib[3..].to_vec()]);
        assert_eq!(frames[0]["content"]["parts"][0]["text"], "zlib");
        assert_eq!(frames[1]["turnComplete"], true);

        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload.as_bytes()).unwrap();
        let raw = encoder.finish().unwrap();
        let frames = frames_through("deflate", std::slice::from_ref(&raw));
        assert_eq!(frames[0]["content"]["parts"][0]["text"], "zlib");

        let brotli_payload = payload.replace("zlib", "brotli");
        let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        encoder.write_all(brotli_payload.as_bytes()).unwrap();
        let compressed = encoder.into_inner();
        let chunks: Vec<Vec<u8>> = compressed.chunks(5).map(<[u8]>::to_vec).collect();
        let frames = frames_through("br", &chunks);
        assert_eq!(frames[0]["content"]["parts"][0]["text"], "brotli");
        assert_eq!(frames[1]["turnComplete"], true);
    }
    #[test]
    fn a_provider_error_event_keeps_its_own_message_and_code() {
        let event = r#"{"error":{"message":"Rate limit reached","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#;
        let error = Sink::new(true).event(event, false).unwrap_err();
        // A symbolic code is not a status, so the frame stays on the transport code.
        assert_eq!(error.code, 502);
        assert!(error.message.contains("Rate limit reached"), "{error:?}");
        assert!(error.message.contains("type=rate_limit_error"), "{error:?}");
        assert!(
            error.message.contains("code=rate_limit_exceeded"),
            "{error:?}"
        );
        // Google reports the HTTP status in `error.code`, which is what the frame must carry.
        let event =
            r#"{"error":{"code":429,"message":"quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        let error = Sink::new(true).event(event, false).unwrap_err();
        assert_eq!(error.code, 429);
        assert!(error.message.contains("quota exceeded"), "{error:?}");
        assert!(
            error.message.contains("status=RESOURCE_EXHAUSTED"),
            "{error:?}"
        );
        // A provider that only says "error" still fails the turn.
        let error = Sink::new(true)
            .event(r#"{"error":"model not found"}"#, false)
            .unwrap_err();
        assert_eq!(error.message, "upstream reported an error: model not found");
        // A null error is not an error.
        assert!(Sink::new(true)
            .event(r#"{"error":null,"choices":[]}"#, false)
            .is_ok());
    }
    #[test]
    fn error_frames_default_to_the_transport_code() {
        assert_eq!(error_frame("boom")["error"]["code"], json!(502));
        assert_eq!(
            error_frame_with(429, "slow down")["error"]["code"],
            json!(429)
        );
        assert_eq!(
            error_frame_with(429, "slow down")["turnComplete"],
            json!(true)
        );
        assert_eq!(
            error_frame("boom")["error"]["status"],
            json!("UPSTREAM_ERROR")
        );
    }
}
