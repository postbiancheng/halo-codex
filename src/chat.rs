use super::{config, errors};
use agent2api_wasm_sdk::{
    self as sdk, http, json, sse::SseDecoder, websocket::WebSocket, Error, Result, Value,
};
use std::collections::BTreeMap;

fn string(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|v| v["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn input_messages(messages: &[Value], include_instructions: bool) -> Result<(String, Vec<Value>)> {
    let mut instructions = Vec::new();
    let mut items = Vec::new();
    for message in messages {
        let role = string(&message["role"]);
        let content = &message["content"];
        match role {
            "system" | "developer" => {
                if include_instructions {
                    instructions.push(text(content));
                }
            }
            "tool" => items.push(json!({"type":"function_call_output", "call_id":message["tool_call_id"],"output":text(content)})),
            "assistant" | "user" => {
                let kind = if role == "assistant" {
                    "output_text"
                } else {
                    "input_text"
                };
                let mut parts = Vec::new();
                if let Some(value) = content.as_str().filter(|s| !s.is_empty()) {
                    parts.push(json!({"type":kind,"text":value}));
                } else if let Some(values) = content.as_array() {
                    for part in values {
                        match string(&part["type"]) {
                            "text" | "input_text" | "output_text" => {
                                parts.push(json!({"type":kind,"text":part["text"]}))
                            }
                            "image_url" if role == "user" => {
                                let url = &part["image_url"];
                                let value = url.get("url").unwrap_or(url);
                                parts.push(json!({"type":"input_image","image_url":value,"detail":url["detail"].as_str().unwrap_or("auto")}));
                            }
                            _ => return Err(Error::http(400, "Codex WebSocket 不支持该消息内容类型")),
                        }
                    }
                }
                if !parts.is_empty() {
                    items.push(json!({"type":"message","role":role,"content":parts}));
                }
                if role == "assistant" {
                    for call in message["tool_calls"].as_array().into_iter().flatten() {
                        items.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
                    }
                }
            }
            _ => return Err(Error::http(400, "Codex WebSocket 不支持该消息角色")),
        }
    }
    Ok((instructions.join("\n\n"), items))
}

fn input(body: &Value) -> Result<(String, Vec<Value>)> {
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| Error::http(400, "缺少 messages"))?;
    input_messages(messages, true)
}

fn request(body: &Value, session: &str, websocket: bool) -> Result<Value> {
    // Codex backend currently rejects store=true ("Store must be set to false"),
    // so previous_response_id chaining is unavailable on this endpoint.
    let (instructions, items) = input(body)?;
    let mut output = if websocket {
        json!({"type":"response.create","model":body["model"],"instructions":instructions,
            "input":items,"stream":true,"store":false,"parallel_tool_calls":true})
    } else {
        json!({"model":body["model"],"instructions":instructions,
            "input":items,"stream":true,"store":false,"parallel_tool_calls":true})
    };
    if let Some(tools) = body["tools"].as_array() {
        let mut converted = Vec::new();
        for tool in tools {
            let mut function = tool["function"]
                .as_object()
                .cloned()
                .ok_or_else(|| Error::http(400, "Codex WebSocket 当前支持 function 工具"))?;
            function.insert("type".into(), json!("function"));
            converted.push(Value::Object(function));
        }
        output["tools"] = json!(converted);
    }
    if let Some(choice) = body.get("tool_choice") {
        output["tool_choice"] = if choice.is_object() {
            json!({"type":"function","name":choice["function"]["name"]})
        } else {
            choice.clone()
        };
    }
    if let Some(value) = body.get("parallel_tool_calls") {
        output["parallel_tool_calls"] = value.clone();
    }
    apply_reasoning(body, config::reasoning_effort()?.as_deref(), &mut output);
    if let Some(format) = body.get("response_format") {
        output["text"] = if format["type"] == "json_schema" {
            let mut schema = format["json_schema"]
                .as_object()
                .cloned()
                .ok_or_else(|| Error::http(400, "缺少 JSON Schema"))?;
            schema.insert("type".into(), json!("json_schema"));
            json!({"format":schema})
        } else {
            json!({"format":format})
        };
    }
    if !session.is_empty() {
        output["prompt_cache_key"] = json!(session);
    }
    Ok(output)
}

struct Tool {
    index: usize,
    saw_delta: bool,
}

struct Stream<'a> {
    model: &'a str,
    id: String,
    tools: BTreeMap<u64, Tool>,
}

impl Stream<'_> {
    fn emit(&self, delta: Value, finish: Value, usage: Option<Value>) -> Result<()> {
        let mut frame = json!({"id":self.id,"object":"chat.completion.chunk","model":self.model,
            "created":sdk::now() / 1000,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        if let Some(value) = usage {
            frame["usage"] = value;
        }
        sdk::stream_event(&frame)
    }

    fn tool(&mut self, event: &Value, declaration: bool) -> Result<()> {
        let key = event["output_index"].as_u64().unwrap_or(0);
        if declaration {
            let item = &event["item"];
            if item["type"] != "function_call" || self.tools.contains_key(&key) {
                return Ok(());
            }
            let index = self.tools.len();
            self.tools.insert(
                key,
                Tool {
                    index,
                    saw_delta: false,
                },
            );
            return self.emit(
                json!({"tool_calls":[{"index":index,"id":item["call_id"],"type":"function",
                "function":{"name":item["name"],"arguments":""}}]}),
                Value::Null,
                None,
            );
        }
        let Some(tool) = self.tools.get_mut(&key) else {
            return Ok(());
        };
        let item = &event["item"];
        let args = if let Some(delta) = event["delta"].as_str() {
            tool.saw_delta = true;
            delta
        } else if !tool.saw_delta {
            tool.saw_delta = true;
            string(&item["arguments"])
        } else {
            ""
        };
        let index = tool.index;
        if !args.is_empty() {
            self.emit(
                json!({"tool_calls":[{"index":index,"function":{"arguments":args}}]}),
                Value::Null,
                None,
            )?;
        }
        Ok(())
    }

    /// Returns `true` when the upstream response finished.
    fn handle(&mut self, event: &Value) -> Result<bool> {
        let kind = string(&event["type"]);
        if matches!(kind, "error" | "response.failed") {
            let err = event
                .get("error")
                .or_else(|| event.pointer("/response/error"))
                .unwrap_or(event);
            let status = event["status"]
                .as_u64()
                .filter(|s| (400..600).contains(s))
                .unwrap_or(502) as u16;
            let class = errors::classify(status, err);
            return Err(Error::http(
                class["status"].as_u64().unwrap_or(status as u64) as u16,
                class["message"]
                    .as_str()
                    .unwrap_or("Codex WebSocket 请求失败"),
            )
            .with_code(err["code"].as_str().unwrap_or("codex_ws_error")));
        }
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => {
                self.emit(json!({"content":event["delta"]}), Value::Null, None)?;
            }
            "response.reasoning_summary_text.delta" => {
                self.emit(
                    json!({"reasoning_content":event["delta"]}),
                    Value::Null,
                    None,
                )?;
            }
            "response.output_item.added" => self.tool(event, true)?,
            "response.output_item.done" | "response.function_call_arguments.delta" => {
                self.tool(event, false)?
            }
            "response.completed" | "response.incomplete" => {
                let u = &event["response"]["usage"];
                let usage = u.is_object().then(|| json!({"prompt_tokens":u["input_tokens"],"completion_tokens":u["output_tokens"],
                    "total_tokens":u["total_tokens"],"prompt_tokens_details":{"cached_tokens":u["input_tokens_details"]["cached_tokens"]}}));
                let finish = if kind == "response.incomplete" {
                    match string(&event["response"]["incomplete_details"]["reason"]) {
                        "max_output_tokens" => "length",
                        "content_filter" => "content_filter",
                        _ => return Err(Error::http(502, "Codex WebSocket 响应未完成")),
                    }
                } else if self.tools.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                self.emit(json!({}), json!(finish), usage)?;
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }
}

fn apply_reasoning(body: &Value, default_effort: Option<&str>, output: &mut Value) {
    if let Some(effort) = body["reasoning_effort"].as_str() {
        output["reasoning"] = json!({"effort": effort});
    } else if body["reasoning"].is_object() {
        output["reasoning"] = body["reasoning"].clone();
    } else if let Some(effort) = default_effort {
        output["reasoning"] = json!({"effort": effort});
    }
}

fn auth_headers(secret: &Value, request_id: &str) -> Result<Value> {
    let token = string(&secret["accessToken"]);
    let account = string(&secret["accountId"]);
    if token.is_empty()
        || account.is_empty()
        || secret["expiresAt"].as_i64().unwrap_or(0) <= sdk::now()
    {
        return Err(Error::http(401, "Codex 登录已过期").with_code("codex_login_required"));
    }
    Ok(json!({
        "Authorization": format!("Bearer {token}"),
        "chatgpt-account-id": account,
        "ChatGPT-Account-Id": account,
        "originator": config::originator(true)?,
        "OpenAI-Beta": "responses_websockets=2026-02-06",
        "session-id": request_id,
        "x-client-request-id": request_id
    }))
}

fn session_key(body: &Value, params: &Value) -> String {
    body["prompt_cache_key"]
        .as_str()
        .or_else(|| params["session"].as_str())
        .filter(|s| !s.is_empty() && s.len() <= 64 && s.bytes().all(|v| v.is_ascii_graphic()))
        .map(str::to_owned)
        .unwrap_or_default()
}

fn request_id(session: &str) -> Result<String> {
    let random = sdk::host("random.bytes", json!({"length":18}))?
        .as_str()
        .unwrap_or_default()
        .replace('+', "-")
        .replace('/', "_");
    Ok(if session.is_empty() {
        random
    } else {
        session.to_string()
    })
}

pub fn forward(body: Value, params: &Value, secret: &Value) -> Result<()> {
    let session = session_key(&body, params);
    let request_id = request_id(&session)?;
    let headers = auth_headers(secret, &request_id)?;
    let model = string(&body["model"]).to_string();
    let mut stream = Stream {
        model: &model,
        id: format!("chatcmpl-{request_id}"),
        tools: BTreeMap::new(),
    };
    match config::transport()?.as_str() {
        config::TRANSPORT_HTTP => forward_http(body, &session, headers, &mut stream),
        _ => forward_ws(body, &session, headers, &mut stream),
    }
}

fn forward_ws(
    body: Value,
    session: &str,
    headers: Value,
    stream: &mut Stream<'_>,
) -> Result<()> {
    let outbound = request(&body, session, true)?;
    let socket = WebSocket::open(
        "wss://chatgpt.com/backend-api/codex/responses",
        headers,
        &[],
    )?;
    socket.send_text(&outbound.to_string())?;
    let mut started = false;
    loop {
        let frame = socket.read()?;
        if frame["type"] != "text" {
            return Err(
                Error::http(502, "Codex WebSocket 在完成前关闭").with_code("codex_ws_closed")
            );
        }
        let event: Value = serde_json::from_str(string(&frame["data"]))
            .map_err(|_| Error::http(502, "Codex WebSocket 返回无效事件"))?;
        if !started {
            sdk::stream_head(200)?;
            stream.emit(json!({"role":"assistant"}), Value::Null, None)?;
            started = true;
        }
        if stream.handle(&event)? {
            return Ok(());
        }
    }
}

fn forward_http(
    body: Value,
    session: &str,
    mut headers: Value,
    stream: &mut Stream<'_>,
) -> Result<()> {
    let outbound = request(&body, session, false)?;
    headers["Content-Type"] = json!("application/json");
    headers["Accept"] = json!("text/event-stream");
    headers["Cache-Control"] = json!("no-cache");
    let mut response = http::open(
        "POST",
        "https://chatgpt.com/backend-api/codex/responses",
        headers,
        Some(&outbound),
    )?;
    let status = response.status;
    let content_type = response.headers["content-type"]
        .as_str()
        .or_else(|| response.headers["Content-Type"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(200..300).contains(&status)
        || content_type.contains("application/json")
        || content_type.contains("text/html")
    {
        let bytes = response.bytes(1024 * 1024)?;
        let value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| {
            json!(String::from_utf8_lossy(&bytes)
                .chars()
                .take(16 * 1024)
                .collect::<String>())
        });
        let class = errors::classify(status, &value);
        return Err(Error::http(
            class["status"].as_u64().unwrap_or(status as u64) as u16,
            class["message"]
                .as_str()
                .unwrap_or("Codex HTTP 请求失败"),
        )
        .with_code("codex_http_error"));
    }

    sdk::stream_head(200)?;
    stream.emit(json!({"role":"assistant"}), Value::Null, None)?;
    let mut decoder = SseDecoder::default();
    while let Some(bytes) = response.read()? {
        for data in decoder.push(&bytes)? {
            if data == "[DONE]" {
                return sdk::stream_bytes(b"data: [DONE]\n\n");
            }
            let event: Value = serde_json::from_str(&data)
                .map_err(|_| Error::http(502, "Codex HTTP 返回无效事件"))?;
            if stream.handle(&event)? {
                return Ok(());
            }
        }
    }
    Err(Error::http(502, "Codex HTTP 流提前中断").with_code("codex_http_closed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_reasoning_prefers_body_then_default() {
        let mut output = json!({});
        apply_reasoning(&json!({"reasoning_effort": "high"}), Some("medium"), &mut output);
        assert_eq!(output["reasoning"]["effort"], "high");

        let mut output = json!({});
        apply_reasoning(&json!({"reasoning": {"effort": "low"}}), Some("medium"), &mut output);
        assert_eq!(output["reasoning"]["effort"], "low");

        let mut output = json!({});
        apply_reasoning(&json!({}), Some("medium"), &mut output);
        assert_eq!(output["reasoning"]["effort"], "medium");

        let mut output = json!({});
        apply_reasoning(&json!({}), None, &mut output);
        assert!(output.get("reasoning").is_none());
    }

}
