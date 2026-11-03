//! Accio GenAI / OpenAI-compatible / Ollama translation. No I/O or credentials.
use serde_json::{json, Value};
use std::collections::HashSet;

fn first(values: &[&Value]) -> Value {
    values
        .iter()
        .find(|v| !v.is_null() && v.as_str() != Some(""))
        .map(|v| (*v).clone())
        .unwrap_or(Value::Null)
}
fn text(v: &Value) -> String {
    v.as_str().map(str::to_owned).unwrap_or_else(|| {
        if v.is_null() {
            String::new()
        } else {
            v.to_string()
        }
    })
}
fn parts(entry: &Value) -> Vec<Value> {
    entry
        .get("parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| {
            entry
                .get("text")
                .and_then(Value::as_str)
                .map(|s| vec![json!({"text":s})])
                .unwrap_or_default()
        })
}
fn joined(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|p| p["thought"] != true)
        .filter_map(|p| {
            p.as_str().map(str::to_owned).or_else(|| {
                first(&[&p["text"], &p["textDelta"]])
                    .as_str()
                    .map(str::to_owned)
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn parsed(v: &Value) -> Value {
    if let Some(s) = v.as_str() {
        if s.trim().is_empty() {
            return json!({});
        }
        serde_json::from_str(s).unwrap_or_else(|_| json!({"__raw":s}))
    } else if v.is_null() {
        json!({})
    } else {
        v.clone()
    }
}
fn call(p: &Value) -> Value {
    first(&[&p["functionCall"], &p["function_call"]])
}
fn result(p: &Value) -> Value {
    first(&[&p["functionResponse"], &p["function_response"]])
}
fn explicit_id(v: &Value) -> Value {
    first(&[
        &v["id"],
        &v["callId"],
        &v["call_id"],
        &v["toolCallId"],
        &v["tool_call_id"],
    ])
}
fn next_id(seq: &mut usize, reserved: &mut HashSet<String>) -> String {
    loop {
        let id = format!("call_{}", *seq);
        *seq += 1;
        if reserved.insert(id.clone()) {
            return id;
        }
    }
}
fn match_id(
    value: &Value,
    name: &str,
    pending: &mut Vec<(String, String)>,
    seq: &mut usize,
    reserved: &mut HashSet<String>,
) -> String {
    let explicit = explicit_id(value);
    let index = pending.iter().position(|(id, n)| {
        if !explicit.is_null() {
            *id == text(&explicit)
        } else {
            name.is_empty() || n == name
        }
    });
    let matched = index.map(|i| pending.remove(i).0);
    if !explicit.is_null() {
        text(&explicit)
    } else {
        matched.unwrap_or_else(|| next_id(seq, reserved))
    }
}
fn user_content(parts: &[Value]) -> Result<Value, String> {
    let mut content = Vec::new();
    let mut image = false;
    for part in parts {
        if part["thought"] == true {
            continue;
        }
        let t = if part.is_string() {
            part.clone()
        } else {
            first(&[&part["text"], &part["textDelta"]])
        };
        if t.is_string() {
            content.push(json!({"type":"text","text":t}));
        }
        let inline = first(&[&part["inlineData"], &part["inline_data"]]);
        let file = first(&[&part["fileData"], &part["file_data"]]);
        let url = if inline.is_object() {
            let mime = text(&first(&[
                &inline["mimeType"],
                &inline["mime_type"],
                &json!("image/png"),
            ]));
            let data = inline["data"].as_str().unwrap_or("");
            if !mime.to_ascii_lowercase().starts_with("image/") {
                content.push(json!({"type":"text","text":format!("[attachment: {mime}, {} base64 chars; content not decoded]", data.len())}));
                continue;
            }
            if mime.len() <= 6
                || !mime[6..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".+-".contains(c))
            {
                return Err("Only image inlineData is supported".into());
            }
            if data.trim().is_empty() {
                return Err("Image inlineData requires base64 data".into());
            }
            Some(format!("data:{mime};base64,{data}"))
        } else if file.is_object() {
            let url = text(&first(&[&file["fileUri"], &file["file_uri"]]));
            let mime = text(&first(&[&file["mimeType"], &file["mime_type"]]));
            if !mime.is_empty() && !mime.to_ascii_lowercase().starts_with("image/") {
                if !url.trim().is_empty() {
                    content.push(json!({"type":"text","text":format!("[attachment: {mime} at {url}; content not fetched]")}));
                }
                continue;
            }
            if !url.to_ascii_lowercase().starts_with("http://")
                && !url.to_ascii_lowercase().starts_with("https://")
            {
                return Err("Image fileData requires an HTTP(S) URL".into());
            }
            Some(url)
        } else {
            None
        };
        if let Some(url) = url {
            image = true;
            content.push(json!({"type":"image_url","image_url":{"url":url}}));
        }
    }
    Ok(if image {
        Value::Array(content)
    } else {
        Value::String(
            content
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })
}
fn messages(body: &Value) -> Result<Vec<Value>, String> {
    let system = first(&[
        &body["systemInstruction"],
        &body["system_instruction"],
        &body["system_instructions"],
        &body["system"],
    ]);
    let system_text = if system.is_string() {
        text(&system)
    } else if let Some(a) = system.as_array() {
        joined(a)
    } else {
        joined(&parts(&system))
    };
    let mut out = Vec::new();
    if !system_text.trim().is_empty() {
        out.push(json!({"role":"system","content":system_text}));
    }
    let contents = body["contents"].as_array().cloned().unwrap_or_default();
    let mut seq = 0;
    let mut reserved = HashSet::new();
    let mut pending = Vec::new();
    for entry in &contents {
        for part in parts(entry) {
            for v in [call(&part), result(&part)] {
                let id = explicit_id(&v);
                if !id.is_null() {
                    reserved.insert(text(&id));
                }
            }
        }
    }
    for entry in contents {
        if !entry.is_object() {
            continue;
        }
        let role = entry["role"]
            .as_str()
            .unwrap_or("user")
            .to_ascii_lowercase();
        let parts = parts(&entry);
        let content_text = joined(&parts);
        let results: Vec<Value> = parts.iter().map(result).filter(Value::is_object).collect();
        let calls: Vec<Value> = parts.iter().map(call).filter(Value::is_object).collect();
        if !results.is_empty() {
            for response in results {
                let name = text(&first(&[
                    &response["name"],
                    &response["functionName"],
                    &response["function_name"],
                ]));
                let raw = first(&[
                    &response["response"],
                    &response["result"],
                    &response["responseJson"],
                    &response["response_json"],
                ]);
                let payload = if response["response"].is_null() && response["result"].is_null() {
                    parsed(&raw)
                } else {
                    raw
                };
                let payload = unwrap_tool_result(payload);
                let id = match_id(&response, &name, &mut pending, &mut seq, &mut reserved);
                let mut message = json!({"role":"tool","tool_call_id":id,"content": if payload.is_string() {text(&payload)} else if payload.is_null(){"{}".into()} else {payload.to_string()}});
                if !name.is_empty() {
                    message["name"] = json!(name);
                }
                out.push(message);
            }
            let content = user_content(&parts)?;
            if content.is_array() || !text(&content).trim().is_empty() {
                out.push(json!({"role":"user","content":content}));
            }
        } else if !calls.is_empty() {
            let mut tool_calls = Vec::new();
            for call in calls {
                let id = explicit_id(&call);
                let id = if id.is_null() {
                    next_id(&mut seq, &mut reserved)
                } else {
                    text(&id)
                };
                let name = text(&first(&[
                    &call["name"],
                    &call["functionName"],
                    &call["function_name"],
                    &json!("tool"),
                ]));
                let args = first(&[
                    &call["args"],
                    &call["arguments"],
                    &call["argsJson"],
                    &call["args_json"],
                ]);
                let parsed_args = parsed(&args);
                let arguments = if parsed_args.get("__raw").is_some() {
                    "{}".into()
                } else {
                    parsed_args.to_string()
                };
                pending.push((id.clone(), name.clone()));
                tool_calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}));
            }
            out.push(json!({"role":"assistant","content":if content_text.trim().is_empty(){Value::Null}else{json!(content_text)},"tool_calls":tool_calls}));
        } else if role == "model" || role == "assistant" {
            let content = user_content(&parts)?;
            if content.is_array() || !text(&content).trim().is_empty() {
                out.push(json!({"role":"assistant","content":content}));
            }
        } else if role == "tool" || role == "function" {
            let id = match_id(
                &entry,
                entry["name"].as_str().unwrap_or(""),
                &mut pending,
                &mut seq,
                &mut reserved,
            );
            out.push(json!({"role":"tool","tool_call_id":id,"content":content_text}));
        } else {
            let content = user_content(&parts)?;
            if content.is_array() || !text(&content).trim().is_empty() {
                out.push(json!({"role":"user","content":content}));
            }
        }
    }
    Ok(out)
}
fn unwrap_tool_result(value: Value) -> Value {
    if let Some(raw) = value.get("__raw") {
        return raw.clone();
    }
    if let Some(object) = value.as_object() {
        if !object.is_empty()
            && object
                .keys()
                .all(|k| ["result", "content", "is_error", "isError"].contains(&k.as_str()))
        {
            for key in ["result", "content"] {
                if let Some(result) = value.get(key).filter(|v| !v.is_null()) {
                    return result.clone();
                }
            }
        }
    }
    value
}
fn resolve_schema(d: &Value) -> Option<Value> {
    let raw = first(&[
        &d["parameters"],
        &d["parameters_json"],
        &d["parametersJson"],
        &d["parametersJSON"],
        &d["parametersJsonSchema"],
        &d["parameters_json_schema"],
        &d["parametersSchema"],
        &d["input_schema"],
        &d["inputSchema"],
        &d["input_json"],
        &d["args"],
    ]);
    if raw.is_null() {
        return None;
    }
    let schema = parsed(&raw);
    (schema.is_object() && schema.get("__raw").is_none()).then_some(schema)
}
fn declarations(node: &Value, depth: usize, out: &mut Vec<Value>) {
    // Bound recursion independently of JSON parsing limits.
    if depth > 32 {
        return;
    }
    if let Some(nodes) = node.as_array() {
        for n in nodes {
            declarations(n, depth + 1, out);
        }
    } else if node.is_object() {
        for key in [
            "functionDeclarations",
            "function_declarations",
            "functions",
            "declarations",
            "tools",
        ] {
            if node[key].is_array() {
                declarations(&node[key], depth + 1, out);
                return;
            }
        }
        out.push(if node["function"].is_object() {
            node["function"].clone()
        } else {
            node.clone()
        });
    }
}
/// Diagnostics for the tool set forwarded upstream.
///
/// Returned alongside the payload so callers can report dropped or unusable declarations
/// instead of silently shipping a model a tool it cannot call.
#[derive(Debug, Default, Clone)]
pub struct ToolReport {
    pub names: Vec<String>,
    pub dropped: Vec<String>,
    pub schema_fallbacks: Vec<String>,
    pub upgraded: Vec<String>,
}
fn tools(body: &Value) -> (Vec<Value>, ToolReport) {
    let mut report = ToolReport::default();
    let mut raw = Vec::new();
    for field in ["tools", "functionDeclarations", "function_declarations"] {
        declarations(&body[field], 0, &mut raw);
    }
    let mut out: Vec<Value> = Vec::new();
    let mut fallback_names = HashSet::new();
    for d in raw {
        let name = text(&first(&[
            &d["name"],
            &d["functionName"],
            &d["function_name"],
            &d["id"],
        ]));
        if name.trim().is_empty() {
            // A declaration with no usable name cannot be called at all, so record the shape
            // instead of dropping it without a trace.
            report.dropped.push(
                d.as_object()
                    .map(|o| o.keys().take(6).cloned().collect::<Vec<_>>().join("|"))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "object".into()),
            );
            continue;
        }
        let schema = resolve_schema(&d);
        if let Some(existing) = out.iter_mut().find(|t| t["function"]["name"] == name) {
            if fallback_names.contains(&name) {
                if let Some(schema) = schema {
                    existing["function"]["parameters"] = schema;
                    if existing["function"]["description"] == "" {
                        existing["function"]["description"] =
                            json!(d["description"].as_str().unwrap_or(""));
                    }
                    fallback_names.remove(&name);
                    report.schema_fallbacks.retain(|n| n != &name);
                    report.upgraded.push(name.clone());
                }
            }
            continue;
        }
        if schema.is_none() {
            fallback_names.insert(name.clone());
            report.schema_fallbacks.push(name.clone());
        }
        out.push(json!({"type":"function","function":{"name":name,"description":d["description"].as_str().unwrap_or(""),"parameters":schema.unwrap_or_else(||json!({"type":"object","properties":{},"additionalProperties":true}))}}));
    }
    report.names = out
        .iter()
        .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
        .collect();
    (out, report)
}
fn number(v: Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        .filter(|n| n.is_finite())
}

fn num_value(n: f64) -> Value {
    if n.fract() == 0. && n >= 0. && n <= u64::MAX as f64 {
        json!(n as u64)
    } else {
        json!(n)
    }
}

pub fn thoughts_requested(body: &Value) -> bool {
    let cfg = first(&[&body["generationConfig"], &body["generation_config"]]);
    let thinking = first(&[&cfg["thinkingConfig"], &cfg["thinking_config"]]);
    let include = first(&[
        &body["includeThoughts"],
        &body["include_thoughts"],
        &cfg["includeThoughts"],
        &cfg["include_thoughts"],
        &thinking["includeThoughts"],
        &thinking["include_thoughts"],
    ]);
    let budget = first(&[
        &body["thinkingBudget"],
        &body["thinking_budget"],
        &cfg["thinkingBudget"],
        &cfg["thinking_budget"],
        &thinking["thinkingBudget"],
        &thinking["thinking_budget"],
    ]);
    include != false && number(budget) != Some(0.)
}

pub fn request_with_report(
    body: &Value,
    model: &Value,
    ollama: bool,
) -> Result<(Value, ToolReport), String> {
    let mut messages = messages(body)?;
    if ollama {
        for message in &mut messages {
            if let Some(content) = message["content"].as_array() {
                let mut texts = Vec::new();
                let mut images = Vec::new();
                for part in content {
                    if part["type"] == "text" {
                        texts.push(text(&part["text"]));
                    } else if part["type"] == "image_url" {
                        let url = part["image_url"]["url"].as_str().unwrap_or("");
                        let (prefix, data) = url.split_once(";base64,").ok_or(
                            "Ollama images require inline base64 data; remote URLs are not fetched",
                        )?;
                        if !prefix.starts_with("data:image/") {
                            return Err("Ollama images require inline base64 data".into());
                        }
                        images.push(json!(data));
                    }
                }
                message["content"] = json!(texts.join("\n"));
                message["images"] = json!(images);
            }
            if message["role"] == "tool" && message["name"].is_string() {
                message["tool_name"] = message["name"].clone();
            }
            if let Some(calls) = message["tool_calls"].as_array() {
                let calls:Vec<Value>=calls.iter().map(|c|json!({"function":{"name":c["function"]["name"],"arguments":parsed(&c["function"]["arguments"])}})).collect();
                message["tool_calls"] = json!(calls);
            }
            message.as_object_mut().unwrap().remove("tool_call_id");
            message.as_object_mut().unwrap().remove("name");
            if message["content"].is_null() {
                message["content"] = json!("");
            }
        }
    }
    let mut out = json!({"model":first(&[&model["upstreamModel"],&model["modelCode"],&model["id"],&body["model"]]),"messages":messages,"stream":body.get("stream").map(|s|s!=&json!(false)).unwrap_or(model["stream"]!=json!(false))});
    if !ollama
        && out["stream"] == true
        && model["provider"].as_str().unwrap_or("openai") == "openai"
    {
        out["stream_options"] = json!({"include_usage":true});
    }
    let cfg = first(&[&body["generationConfig"], &body["generation_config"]]);
    let mut options = json!({});
    for (names, openai, native) in [
        (vec!["temperature"], "temperature", "temperature"),
        (
            vec!["maxOutputTokens", "max_output_tokens", "maxTokens"],
            "max_tokens",
            "num_predict",
        ),
        (vec!["topP", "top_p"], "top_p", "top_p"),
        (vec!["topK", "top_k"], "top_k", "top_k"),
    ] {
        let values: Vec<&Value> = names
            .iter()
            .map(|n| &body[*n])
            .chain(names.iter().map(|n| &cfg[*n]))
            .collect();
        if let Some(mut n) = number(first(&values)) {
            if openai == "temperature" {
                n = n.clamp(0., 2.);
            }
            if native == "num_predict" {
                n = n.floor();
                if n <= 0. {
                    continue;
                }
            }
            if ollama {
                options[native] = num_value(n);
            } else if !openai.is_empty() {
                out[openai] = num_value(n);
            }
        }
    }
    let stop = first(&[
        &body["stopSequences"],
        &body["stop_sequences"],
        &body["stop"],
        &cfg["stopSequences"],
        &cfg["stop_sequences"],
        &cfg["stop"],
    ]);
    let stops = if let Some(a) = stop.as_array() {
        a.clone()
    } else {
        vec![stop]
    };
    let stops: Vec<Value> = stops
        .into_iter()
        .filter(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
        .collect();
    if !stops.is_empty() {
        if ollama {
            options["stop"] = json!(stops);
        } else {
            out["stop"] = json!(stops);
        }
    }
    if ollama && options.as_object().is_some_and(|o| !o.is_empty()) {
        out["options"] = options;
    }
    if !ollama {
        let effort = first(&[
            &body["reasoningEffort"],
            &body["reasoning_effort"],
            &body["properties"]["reasoning_effort"],
            &body["properties"]["reasoningEffort"],
            &cfg["reasoningEffort"],
            &cfg["reasoning_effort"],
        ]);
        if effort.as_str().is_some_and(|s| !s.trim().is_empty()) {
            out["reasoning_effort"] = effort;
        }
        let format = first(&[
            &body["responseFormat"],
            &body["response_format"],
            &cfg["responseFormat"],
            &cfg["response_format"],
        ]);
        if let Some(format) = format.as_str() {
            let format = format.to_ascii_lowercase();
            if format == "json_schema" {
                return Err("responseFormat=json_schema requires a complete JSON schema; this shorthand is not supported".into());
            }
            if ["json_object", "text"].contains(&format.as_str()) {
                out["response_format"] = json!({"type":format});
            }
        }
    }
    let (declarations, report) = tools(body);
    if !declarations.is_empty() {
        out["tools"] = json!(declarations);
        if !ollama {
            let choice = first(&[
                &body["toolChoice"],
                &body["tool_choice"],
                &cfg["toolChoice"],
                &cfg["tool_choice"],
            ]);
            if choice.as_str().is_some_and(|s| !s.trim().is_empty()) {
                out["tool_choice"] = choice;
            }
        }
    }
    Ok((out, report))
}
/// Payload-only shorthand. The gateway itself uses [`request_with_report`] so it can log
/// the forwarded tool set; this stays for the protocol tests, which assert payloads.
#[cfg(test)]
pub fn request(body: &Value, model: &Value, ollama: bool) -> Result<Value, String> {
    request_with_report(body, model, ollama).map(|(payload, _)| payload)
}
fn reason(reason: Option<&str>) -> &'static str {
    match reason.unwrap_or("").to_ascii_lowercase().as_str() {
        "length" | "max_tokens" => "MAX_TOKENS",
        "tool_calls" | "tool_use" => "TOOL_CALLS",
        "content_filter" => "SAFETY",
        _ => "STOP",
    }
}
fn usage(value: &Value) -> Value {
    let prompt = number(first(&[
        &value["prompt_tokens"],
        &value["promptTokenCount"],
        &value["prompt_eval_count"],
    ]));
    let completion = number(first(&[
        &value["completion_tokens"],
        &value["candidatesTokenCount"],
        &value["eval_count"],
    ]));
    let total = number(first(&[&value["total_tokens"], &value["totalTokenCount"]]))
        .unwrap_or(prompt.unwrap_or(0.) + completion.unwrap_or(0.));
    let mut out = json!({"totalTokenCount":num_value(total)});
    if let Some(n) = prompt {
        out["promptTokenCount"] = num_value(n);
    }
    if let Some(n) = completion {
        out["candidatesTokenCount"] = num_value(n);
    }
    out
}
pub fn text_frame(text: &str) -> Value {
    json!({"content":{"role":"model","parts":[{"text":text}]},"turnComplete":false})
}
pub fn done_frame(tokens: Option<Value>, finish: Option<&str>) -> Value {
    let mut out = json!({"content":{"role":"model","parts":[]},"turnComplete":true,"finishReason":reason(finish)});
    if let Some(v) = tokens {
        if v.is_object() {
            out["usageMetadata"] = usage(&v);
        }
    }
    out
}
fn call_args(value: &Value) -> (Value, String) {
    if value.is_object() {
        return (value.clone(), value.to_string());
    }
    let raw = value.as_str().unwrap_or("").trim();
    if raw.is_empty() {
        return (json!({}), "{}".into());
    }
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        if value.is_object() {
            return (value.clone(), value.to_string());
        }
    }
    (json!({}), raw.into())
}
pub fn thought_frame(text: &str) -> Value {
    json!({"content":{"role":"model","parts":[{"text":text,"thought":true}]},"turnComplete":false})
}
pub fn calls_frame(calls: Vec<Value>) -> Value {
    let parts: Vec<Value> = calls
        .into_iter()
        .map(|c| {
            let (args, args_json) = call_args(&c["args"]);
            let mut call = json!({"name":c["name"],"args":args,"argsJson":args_json});
            if !c["id"].is_null() {
                call["id"] = c["id"].clone();
            }
            json!({"functionCall":call})
        })
        .collect();
    json!({"content":{"role":"model","parts":parts},"turnComplete":true,"finishReason":"TOOL_CALLS"})
}
pub fn response(body: &Value, ollama: bool) -> Result<Value, String> {
    if !body["error"].is_null() {
        return Err(body["error"]["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| text(&body["error"])));
    }
    let message = if ollama {
        &body["message"]
    } else {
        &body["choices"][0]["message"]
    };
    if !message.is_object() {
        return Err("upstream response has no completion message".into());
    }
    let finish = if ollama {
        body["done_reason"].as_str()
    } else {
        body["choices"][0]["finish_reason"].as_str()
    };
    let mut out = done_frame(
        if ollama {
            Some(body.clone())
        } else {
            body.get("usage").cloned()
        },
        finish,
    );
    let mut parts = Vec::new();
    let reasoning = first(&[
        &message["reasoning_content"],
        &message["reasoning"],
        &message["thinking"],
    ]);
    if let Some(s) = reasoning.as_str().filter(|s| !s.is_empty()) {
        parts.push(json!({"text":s,"thought":true}));
    }
    if let Some(s) = message["content"].as_str() {
        if !s.is_empty() {
            parts.push(json!({"text":s}));
        }
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        for c in calls {
            let (args, args_json) = call_args(&c["function"]["arguments"]);
            let mut call = json!({"name":c["function"]["name"],"args":args,"argsJson":args_json});
            if !c["id"].is_null() {
                call["id"] = c["id"].clone();
            }
            parts.push(json!({"functionCall":call}));
        }
        if !calls.is_empty() {
            out["finishReason"] = json!("TOOL_CALLS");
        }
    }
    if parts.is_empty() {
        return Err("upstream returned an empty completion message".into());
    }
    out["content"]["parts"] = json!(parts);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_continuation_fifo_and_explicit_ids() {
        let body = json!({"contents":[{"role":"model","parts":[{"functionCall":{"name":"read"}},{"functionCall":{"name":"read","id":"call_0"}}]},{"parts":[{"functionResponse":{"name":"read","id":"call_0","response":"second"}},{"functionResponse":{"name":"read","response":"first"}}]}]});
        let p = request(&body, &json!({"id":"x"}), false).unwrap();
        let m = &p["messages"];
        assert_eq!(m[1]["tool_call_id"], "call_0");
        assert_eq!(m[2]["tool_call_id"], m[0]["tool_calls"][0]["id"]);
        assert_ne!(m[0]["tool_calls"][0]["id"], "call_0");
    }
    #[test]
    fn inline_images_and_remote_ollama_rejection() {
        let b = json!({"contents":[{"parts":[{"text":"look"},{"inline_data":{"mime_type":"image/png","data":"YWJj"}}]}]});
        let p = request(&b, &json!({}), true).unwrap();
        assert_eq!(p["messages"][0]["images"], json!(["YWJj"]));
        let b =
            json!({"contents":[{"parts":[{"fileData":{"fileUri":"https://example.test/x.png"}}]}]});
        assert!(request(&b, &json!({}), true).is_err());
        assert!(request(&b, &json!({}), false).is_ok());
    }
    #[test]
    fn non_image_is_explicit_metadata_only() {
        let b = json!({"contents":[{"parts":[{"inlineData":{"mimeType":"application/pdf","data":"YWJj"}}]}]});
        let p = request(&b, &json!({}), false).unwrap();
        assert!(p["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("content not decoded"));
        assert!(!p.to_string().contains("YWJj"));
    }
    #[test]
    fn aliases_and_knobs() {
        let b = json!({"generation_config":{"temperature":7,"max_output_tokens":12},"contents":[{"role":"model","parts":[{"function_call":{"name":"read","args_json":"{\"x\":1}"}}]},{"parts":[{"function_response":{"name":"read","response_json":"error text"}}]}]});
        let p = request(&b, &json!({}), false).unwrap();
        assert_eq!(p["temperature"], 2.);
        assert_eq!(p["messages"][1]["content"], "error text");
        assert_eq!(
            p["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
    }
    #[test]
    fn nonstream_tools_usage_and_errors() {
        let p=response(&json!({"choices":[{"message":{"tool_calls":[{"id":"a","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3}}),false).unwrap();
        assert_eq!(p["finishReason"], "TOOL_CALLS");
        assert_eq!(p["content"]["parts"][0]["functionCall"]["id"], "a");
        assert_eq!(p["usageMetadata"]["totalTokenCount"], 5.);
        assert!(response(&json!({"error":{"message":"denied"}}), false).is_err());
    }
    #[test]
    fn text_only_keeps_string() {
        let p = request(
            &json!({"systemInstruction":"sys","contents":[{"parts":[{"text":"a"},{"text":"b"}]}]}),
            &json!({"id":"test"}),
            false,
        )
        .unwrap();
        assert_eq!(p["messages"][0]["role"], "system");
        assert_eq!(p["messages"][1]["content"], "a\nb");
        assert_eq!(p["model"], "test");
    }
}

#[cfg(test)]
mod golden {
    use super::*;
    #[test]
    fn matches_tested_node_converter_fixtures() {
        let fixtures: Value = serde_json::from_str(include_str!("protocol-fixtures.json")).unwrap();
        for fixture in fixtures.as_array().unwrap() {
            let actual = request(
                &fixture["body"],
                &fixture["model"],
                fixture["ollama"].as_bool().unwrap(),
            )
            .unwrap();
            assert_eq!(
                actual, fixture["expected"],
                "fixture {} ollama={}",
                fixture["name"], fixture["ollama"]
            );
        }
    }
}

#[cfg(test)]
mod adk_0372_tests {
    use super::*;
    #[test]
    fn snake_case_result_envelope_and_ids() {
        let p=request(&json!({"contents":[{"role":"model","parts":[{"function_call":{"call_id":"a","function_name":"read","args_json":"{\"x\":1}"}}]},{"parts":[{"function_response":{"tool_call_id":"a","name":"read","response_json":"{\"result\":\"done\",\"is_error\":false}"}}]}]}),&json!({}),false).unwrap();
        assert_eq!(p["messages"][0]["tool_calls"][0]["id"], "a");
        assert_eq!(p["messages"][1]["tool_call_id"], "a");
        assert_eq!(p["messages"][1]["content"], "done");
        assert_eq!(
            unwrap_tool_result(json!({"result":"x","other":1})),
            json!({"result":"x","other":1})
        );
    }
    #[test]
    fn thought_history_is_never_replayed() {
        let p=request(&json!({"contents":[{"role":"model","parts":[{"text":"private trace","thought":true}]},{"role":"model","parts":[{"text":"trace","thought":true},{"text":"answer"}]},{"parts":[{"text":"hello"}]}]}),&json!({}),false).unwrap();
        assert_eq!(p["messages"].as_array().unwrap().len(), 2);
        assert_eq!(p["messages"][0]["content"], "answer");
        assert!(!p.to_string().contains("trace"));
    }
    #[test]
    fn nested_schema_aliases_upgrade_only_placeholders() {
        let schema = json!({"type":"object","properties":{"x":{"type":"string"}}});
        for key in [
            "parameters",
            "parameters_json",
            "parametersJson",
            "parametersJSON",
            "parametersJsonSchema",
            "parameters_json_schema",
            "parametersSchema",
            "input_schema",
            "inputSchema",
            "input_json",
            "args",
        ] {
            let mut declaration = json!({"function_name":"read","description":"Read"});
            declaration[key] = json!(schema.to_string());
            let (t, _) = tools(
                &json!({"tools":[{"name":"read"},{"functions":[{"declarations":[declaration]}]}]}),
            );
            assert_eq!(t.len(), 1);
            assert_eq!(t[0]["function"]["parameters"], schema, "{key}");
            assert_eq!(t[0]["function"]["description"], "Read");
        }
        let (t, report) = tools(&json!({"tools":[{"name":"read","parameters_json":"bad"}]}));
        assert_eq!(t[0]["function"]["parameters"]["additionalProperties"], true);
        // The permissive fallback is reported so a logged tool set is actionable.
        assert_eq!(report.schema_fallbacks, vec!["read"]);
        assert_eq!(report.names, vec!["read"]);
    }
    #[test]
    fn tool_report_flags_dropped_and_upgraded_declarations() {
        let (t, report) = tools(&json!({"tools":[{"functionDeclarations":[
            {"name":"placeholder"},
            {"name":"placeholder","description":"real","parameters":{"type":"object","properties":{"q":{"type":"string"}}}},
            {"description":"nameless"}
        ]}]}));
        assert_eq!(t.len(), 1);
        assert_eq!(report.upgraded, vec!["placeholder"]);
        assert!(report.schema_fallbacks.is_empty(), "{report:?}");
        assert_eq!(report.dropped, vec!["description"]);
        assert_eq!(report.names, vec!["placeholder"]);
    }
    #[test]
    fn generation_options_and_thinking_precedence() {
        let p=request(&json!({"topK":9,"properties":{"reasoning_effort":"high"},"responseFormat":"JSON_OBJECT","toolChoice":"required","tools":[{"name":"x"}]}),&json!({}),false).unwrap();
        assert!(request(&json!({"responseFormat":"json_schema"}), &json!({}), false).is_err());
        assert_eq!(p["top_k"], 9);
        assert_eq!(p["reasoning_effort"], "high");
        assert_eq!(p["response_format"], json!({"type":"json_object"}));
        assert_eq!(p["tool_choice"], "required");
        assert!(thoughts_requested(&json!({})));
        assert!(!thoughts_requested(
            &json!({"generationConfig":{"thinkingConfig":{"includeThoughts":false}}})
        ));
        assert!(thoughts_requested(
            &json!({"includeThoughts":true,"generationConfig":{"thinkingConfig":{"includeThoughts":false}}})
        ));
        assert!(response(&json!({}), false).is_err());
        assert!(response(&json!({"choices":[{"message":{"content":""}}]}), false).is_err());
        assert!(!thoughts_requested(&json!({"include_thoughts":false})));
        assert!(!thoughts_requested(
            &json!({"generation_config":{"thinking_budget":"0"}})
        ));
        assert!(thoughts_requested(
            &json!({"thinkingBudget":10,"generationConfig":{"thinkingBudget":0}})
        ));
        assert!(thoughts_requested(
            &json!({"includeThoughts":true,"generationConfig":{"includeThoughts":false}})
        ));
    }
    #[test]
    fn malformed_args_keep_raw_output_but_not_invalid_history() {
        for raw in ["{bad", "[]", "1"] {
            let f = calls_frame(vec![json!({"id":"a","name":"x","args":raw})]);
            assert_eq!(f["content"]["parts"][0]["functionCall"]["args"], json!({}));
            assert_eq!(f["content"]["parts"][0]["functionCall"]["argsJson"], raw);
        }
        let p=request(&json!({"contents":[{"role":"model","parts":[{"function_call":{"name":"x","args_json":"{bad"}}]}]}),&json!({}),false).unwrap();
        assert_eq!(
            p["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }
    #[test]
    fn reasoning_nonstream_is_a_thought_part() {
        let r = response(
            &json!({"choices":[{"message":{"reasoning_content":"trace","content":"answer"}}]}),
            false,
        )
        .unwrap();
        assert_eq!(
            r["content"]["parts"][0],
            json!({"text":"trace","thought":true})
        );
        assert_eq!(r["content"]["parts"][1], json!({"text":"answer"}));
        assert_eq!(
            thought_frame("trace")["content"]["parts"][0]["thought"],
            true
        );
    }
    #[test]
    fn document_metadata_does_not_fetch_or_inline_document() {
        let p=request(&json!({"contents":[{"parts":[{"file_data":{"mime_type":"application/pdf","file_uri":"https://example.test/a.pdf"}}]}]}),&json!({}),false).unwrap();
        assert!(p["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("content not fetched"));
        assert!(!p.to_string().contains("image_url"));
    }
}
