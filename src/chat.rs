use super::{config, errors, models};
use agent2api_wasm_sdk::{
    self as sdk, http, json, sse::SseDecoder, websocket::WebSocket, Error, Result, Value,
};
use sha2::{Digest, Sha256};
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

fn tool_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'.' | b'-'))
}

fn require_tool_name(value: Option<&Value>) -> Result<&str> {
    let name = value.and_then(Value::as_str).unwrap_or("").trim();
    if tool_name_ok(name) {
        Ok(name)
    } else {
        Err(Error::http(
            400,
            "Codex 工具名称只能包含英文字母、数字、下划线、点和连字符",
        ))
    }
}

fn convert_tools(tools: &[Value]) -> Result<Vec<Value>> {
    let mut converted = Vec::with_capacity(tools.len());
    for tool in tools {
        let kind = string(&tool["type"]);
        match kind {
            "" | "function" if tool.get("function").is_some_and(Value::is_object) => {
                let mut function = tool["function"]
                    .as_object()
                    .cloned()
                    .ok_or_else(|| Error::http(400, "Halo Codex function 工具缺少定义"))?;
                require_tool_name(function.get("name"))?;
                function.insert("type".into(), json!("function"));
                converted.push(Value::Object(function));
            }
            "" | "function" => {
                require_tool_name(tool.get("name"))?;
                let mut native = tool.clone();
                native["type"] = json!("function");
                converted.push(native);
            }
            "custom" | "namespace" => {
                require_tool_name(tool.get("name"))?;
                converted.push(tool.clone());
            }
            "web_search" => converted.push(tool.clone()),
            _ => {
                return Err(Error::http(
                    400,
                    format!("Halo Codex 不支持工具类型 {kind}"),
                ));
            }
        }
    }
    Ok(converted)
}

fn convert_tool_choice(choice: &Value) -> Value {
    if let Some(name) = choice.pointer("/function/name") {
        json!({"type":"function","name":name})
    } else {
        choice.clone()
    }
}

fn shorten_call_id(id: &str) -> String {
    let id = id.trim();
    if id.len() <= 64 {
        id.to_string()
    } else {
        format!("{:x}", Sha256::digest(id.as_bytes()))
    }
}

fn normalize_native_item(mut item: Value) -> Result<Option<Value>> {
    let Some(fields) = item.as_object_mut() else {
        return Err(Error::http(400, "Halo Codex input 的每一项必须是对象"));
    };
    let kind = fields
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
        .to_string();
    if kind == "item_reference" {
        return Err(
            Error::http(400, "Codex 订阅不保存历史引用，请发送完整历史条目")
                .with_code("codex_stateless_history_required"),
        );
    }
    if kind == "reasoning" {
        let encrypted = fields
            .get("encrypted_content")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        if encrypted.is_none() {
            // Plaintext reasoning is not replayable on the subscription endpoint.
            return Ok(None);
        }
        fields.remove("content");
    } else if !matches!(
        kind.as_str(),
        "message"
            | "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "web_search_call"
            | "local_shell_call"
            | "local_shell_call_output"
            | "additional_tools"
            | "tool_search_call"
            | "tool_search_output"
    ) {
        return Ok(None);
    }

    let notification = matches!(
        kind.as_str(),
        "function_call_output" | "custom_tool_call_output"
    ) && fields
        .get("call_id")
        .and_then(Value::as_str)
        .is_none_or(|value| value.is_empty());
    if !notification {
        fields.remove("id");
        if matches!(kind.as_str(), "message" | "reasoning") {
            fields.remove("status");
        }
    }
    if matches!(
        kind.as_str(),
        "function_call"
            | "function_call_output"
            | "local_shell_call"
            | "local_shell_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "tool_search_call"
            | "tool_search_output"
    ) {
        if let Some(id) = fields.get("call_id").and_then(Value::as_str) {
            let shortened = shorten_call_id(id);
            if shortened != id {
                fields.insert("call_id".into(), json!(shortened));
            }
        }
    }
    if kind == "message" {
        let role = fields.get("role").and_then(Value::as_str);
        if let Some(content) = fields.get("content").and_then(Value::as_str) {
            let content_type = if role == Some("assistant") {
                "output_text"
            } else {
                "input_text"
            };
            let parts = json!([{"type":content_type,"text":content}]);
            fields.insert("content".into(), parts);
        }
        if fields.get("role").and_then(Value::as_str) == Some("system") {
            fields.insert("role".into(), json!("developer"));
        }
    }
    Ok(Some(item))
}

fn passthrough_input_item(message: &Value) -> Result<Option<Value>> {
    let kind = string(&message["type"]);
    if kind.is_empty() {
        return Ok(None);
    }
    normalize_native_item(message.clone())
}

fn ensure_encrypted_include(output: &mut Value) {
    let active = output
        .get("reasoning")
        .and_then(|value| value.get("effort"))
        .and_then(Value::as_str)
        .is_some_and(|effort| !matches!(effort, "" | "none" | "off"));
    if !active {
        return;
    }
    if let Some(fields) = output["reasoning"].as_object_mut() {
        fields
            .entry("summary".to_string())
            .or_insert_with(|| json!("auto"));
    }
    let include = output.as_object_mut().map(|fields| {
        fields
            .entry("include".to_string())
            .or_insert_with(|| json!([]))
    });
    let Some(Value::Array(include)) = include else {
        return;
    };
    if !include
        .iter()
        .any(|value| value.as_str() == Some("reasoning.encrypted_content"))
    {
        include.push(json!("reasoning.encrypted_content"));
    }
}

fn declare_search(output: &mut Value) {
    let Some(items) = output["input"].as_array() else {
        return;
    };
    if !items
        .iter()
        .any(|item| string(&item["type"]) == "web_search_call")
    {
        return;
    }
    let mut tools = output["tools"].as_array().cloned().unwrap_or_default();
    let already = tools.iter().any(|tool| {
        let kind = string(&tool["type"]);
        kind.starts_with("web_search")
            || (kind == "function" && string(&tool["name"]) == "web.run")
            || (kind == "namespace" && string(&tool["name"]) == "web")
    });
    if already {
        return;
    }
    tools.push(json!({ "type": "web_search", "external_web_access": false }));
    output["tools"] = Value::Array(tools);
}

fn reject_stateless_history(body: &Value) -> Result<()> {
    for key in ["previous_response_id", "conversation", "prompt"] {
        if body
            .get(key)
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
        {
            return Err(Error::http(
                400,
                format!("Codex 订阅不保存服务端历史，不能使用 {key}，请在 messages/input 中发送完整历史"),
            )
            .with_code("codex_stateless_history_required"));
        }
    }
    if body.get("background").and_then(Value::as_bool) == Some(true) {
        return Err(Error::http(
            400,
            "Codex 订阅不支持后台生成，请使用普通流式请求",
        ));
    }
    Ok(())
}

fn input_messages(messages: &[Value], include_instructions: bool) -> Result<(String, Vec<Value>)> {
    let mut instructions = Vec::new();
    let mut items = Vec::new();
    for message in messages {
        if let Some(item) = passthrough_input_item(message)? {
            items.push(item);
            continue;
        }
        let role = string(&message["role"]);
        let content = &message["content"];
        match role {
            "system" | "developer" => {
                if include_instructions {
                    instructions.push(text(content));
                }
            }
            "tool" => {
                let call_id = message
                    .get("tool_call_id")
                    .or_else(|| message.get("call_id"))
                    .and_then(Value::as_str)
                    .map(shorten_call_id)
                    .map(Value::String)
                    .unwrap_or(Value::Null);
                let kind = if string(&message["type"]) == "custom_tool_call_output" {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                items.push(json!({"type":kind,"call_id":call_id,"output":text(content)}));
            }
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
                            _ => return Err(Error::http(400, "Halo Codex 不支持该消息内容类型")),
                        }
                    }
                }
                if !parts.is_empty() {
                    items.push(json!({"type":"message","role":role,"content":parts}));
                }
                if role == "assistant" {
                    for call in message["tool_calls"].as_array().into_iter().flatten() {
                        let call_type = string(&call["type"]);
                        let call_id = call
                            .get("id")
                            .and_then(Value::as_str)
                            .map(shorten_call_id)
                            .map(Value::String)
                            .unwrap_or(Value::Null);
                        if call_type == "custom" || call.get("custom").is_some_and(Value::is_object)
                        {
                            let custom = call.get("custom").unwrap_or(call);
                            items.push(json!({
                                "type":"custom_tool_call",
                                "call_id": call_id,
                                "name": custom.get("name").cloned().unwrap_or(Value::Null),
                                "input": custom.get("input").or_else(|| custom.get("arguments")).cloned().unwrap_or(json!("")),
                            }));
                        } else {
                            items.push(json!({
                                "type":"function_call",
                                "call_id": call_id,
                                "name": call.pointer("/function/name").cloned().unwrap_or(Value::Null),
                                "arguments": call.pointer("/function/arguments").cloned().unwrap_or(json!("")),
                            }));
                        }
                    }
                }
            }
            _ => return Err(Error::http(400, "Halo Codex 不支持该消息角色")),
        }
    }
    Ok((instructions.join("\n\n"), items))
}

fn input(body: &Value) -> Result<(String, Vec<Value>)> {
    let instructions = || string(&body["instructions"]).to_owned();
    if let Some(text) = body["input"].as_str() {
        return Ok((
            instructions(),
            vec![
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
            ],
        ));
    }
    if let Some(items) = body["input"].as_array() {
        let mut normalized = Vec::with_capacity(items.len());
        for item in items {
            if let Some(pass) = passthrough_input_item(item)? {
                normalized.push(pass);
            } else if matches!(item["role"].as_str(), Some("system" | "developer")) {
                let mut message = item.clone();
                message["type"] = json!("message");
                normalized.extend(normalize_native_item(message)?);
            } else if item.get("role").is_some() {
                let (_ignored, mut rows) = input_messages(std::slice::from_ref(item), false)?;
                normalized.append(&mut rows);
            } else if string(&item["type"]) == "reasoning" {
                // Plaintext reasoning already dropped by normalize_native_item.
                continue;
            } else {
                return Err(Error::http(400, "Halo Codex input 条目缺少有效 type"));
            }
        }
        return Ok((instructions(), normalized));
    }
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| Error::http(400, "缺少 messages 或 input"))?;
    input_messages(messages, true)
}

fn apply_catalog_defaults(body: &Value, output: &mut Value) {
    let model = string(&body["model"]);
    let Some(profile) = models::profile_for(model) else {
        return;
    };
    if string(&output["instructions"]).trim().is_empty() {
        if let Some(base) = profile["baseInstructions"]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            output["instructions"] = json!(base);
        }
    }
    if !body["text"].is_object()
        && body.get("response_format").is_none()
        && profile["supportVerbosity"] == true
    {
        if let Some(verbosity) = profile["defaultVerbosity"]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            output["text"] = json!({ "verbosity": verbosity });
        }
    }
}

fn request(body: &Value, session: &str, websocket: bool) -> Result<Value> {
    // Codex backend currently rejects store=true ("Store must be set to false"),
    // so previous_response_id chaining is unavailable on this endpoint.
    reject_stateless_history(body)?;
    let (instructions, items) = input(body)?;
    let mut output = if websocket {
        json!({"type":"response.create","model":body["model"],"instructions":instructions,
            "input":items,"stream":true,"store":false,"parallel_tool_calls":true})
    } else {
        json!({"model":body["model"],"instructions":instructions,
            "input":items,"stream":true,"store":false,"parallel_tool_calls":true})
    };
    apply_catalog_defaults(body, &mut output);
    if let Some(tools) = body["tools"].as_array() {
        output["tools"] = json!(convert_tools(tools)?);
        if body
            .get("tool_choice")
            .is_none_or(|choice| choice.as_str() == Some(""))
        {
            output["tool_choice"] = json!("auto");
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        if choice.as_str() != Some("") {
            output["tool_choice"] = convert_tool_choice(choice);
        }
    }
    if let Some(value) = body.get("parallel_tool_calls") {
        output["parallel_tool_calls"] = value.clone();
    }
    if let Some(include) = body["include"].as_array() {
        output["include"] = Value::Array(include.clone());
    }
    if let Some(tier) = body["service_tier"].as_str() {
        if matches!(tier, "priority" | "ultrafast") {
            output["service_tier"] = json!(tier);
        }
    }
    apply_reasoning(body, config::reasoning_effort()?.as_deref(), &mut output);
    ensure_encrypted_include(&mut output);
    declare_search(&mut output);
    if let Some(text) = body["text"].as_object() {
        output["text"] = json!(text);
    } else if let Some(format) = body.get("response_format") {
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
    started: bool,
}

impl Stream<'_> {
    fn emit(&mut self, delta: Value, finish: Value, usage: Option<Value>) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.emit_chunk(json!({"role":"assistant"}), Value::Null, None)?;
            self.started = true;
        }
        self.emit_chunk(delta, finish, usage)
    }

    fn emit_chunk(&self, delta: Value, finish: Value, usage: Option<Value>) -> Result<()> {
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
            let kind = string(&item["type"]);
            if !matches!(kind, "function_call" | "custom_tool_call")
                || self.tools.contains_key(&key)
            {
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
            item["arguments"]
                .as_str()
                .or_else(|| item["input"].as_str())
                .unwrap_or("")
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
                .filter(|value| !value.is_null())
                .or_else(|| {
                    event
                        .pointer("/response/error")
                        .filter(|value| !value.is_null())
                })
                .unwrap_or(event);
            let status = event["status"]
                .as_u64()
                .filter(|s| (400..600).contains(s))
                .unwrap_or(502) as u16;
            let class = errors::classify(status, err);
            return Err(Error::http(
                class["status"].as_u64().unwrap_or(status as u64) as u16,
                class["message"].as_str().unwrap_or("Halo Codex 请求失败"),
            )
            .with_code(err["code"].as_str().unwrap_or("codex_ws_error"))
            .with_reset(class["reset_at"].as_i64()));
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
            "response.output_item.done"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta" => self.tool(event, false)?,
            "response.completed" | "response.incomplete" => {
                let u = &event["response"]["usage"];
                let usage = u.is_object().then(|| json!({"prompt_tokens":u["input_tokens"],"completion_tokens":u["output_tokens"],
                    "total_tokens":u["total_tokens"],"prompt_tokens_details":{"cached_tokens":u["input_tokens_details"]["cached_tokens"]}}));
                let finish = if kind == "response.incomplete" {
                    match string(&event["response"]["incomplete_details"]["reason"]) {
                        "max_output_tokens" => "length",
                        "content_filter" => "content_filter",
                        _ => return Err(Error::http(502, "Halo Codex 响应未完成")),
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

fn normalize_effort(effort: &str) -> &str {
    if effort.eq_ignore_ascii_case("ultra") {
        "max"
    } else {
        effort
    }
}

fn apply_reasoning(body: &Value, default_effort: Option<&str>, output: &mut Value) {
    if let Some(effort) = body["reasoning_effort"].as_str() {
        output["reasoning"] = json!({"effort": normalize_effort(effort)});
    } else if body["reasoning"].is_object() {
        let mut reasoning = body["reasoning"].clone();
        if let Some(effort) = reasoning["effort"].as_str() {
            reasoning["effort"] = json!(normalize_effort(effort));
        }
        output["reasoning"] = reasoning;
    } else if let Some(effort) = default_effort {
        output["reasoning"] = json!({"effort": normalize_effort(effort)});
    }
}

fn auth_headers(secret: &Value, request_id: &str, websocket: bool) -> Result<Value> {
    let token = string(&secret["accessToken"]);
    let account = string(&secret["accountId"]);
    if token.is_empty()
        || account.is_empty()
        || secret["expiresAt"].as_i64().unwrap_or(0) <= sdk::now()
    {
        return Err(Error::http(401, "Codex 登录已过期").with_code("codex_login_required"));
    }
    let originator = config::originator(true)?;
    let version = config::client_version()?;
    let mut headers = json!({
        "Authorization": format!("Bearer {token}"),
        "chatgpt-account-id": account,
        "ChatGPT-Account-Id": account,
        "originator": originator,
        "User-Agent": config::user_agent(true)?,
        "version": version,
        "session-id": request_id,
        "thread-id": request_id,
        "x-client-request-id": request_id
    });
    headers["OpenAI-Beta"] = json!(if websocket {
        "responses_websockets=2026-02-06"
    } else {
        "responses=experimental"
    });
    Ok(headers)
}

fn session_key(body: &Value, params: &Value) -> String {
    body["prompt_cache_key"]
        .as_str()
        .or_else(|| params["session"].as_str())
        .filter(|s| !s.is_empty() && s.len() <= 512 && s.bytes().all(|v| v.is_ascii_graphic()))
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
    let model = string(&body["model"]).to_string();
    let mut stream = Stream {
        model: &model,
        id: format!("chatcmpl-{request_id}"),
        tools: BTreeMap::new(),
        started: false,
    };
    match config::transport()?.as_str() {
        config::TRANSPORT_HTTP => {
            let headers = auth_headers(secret, &request_id, false)?;
            forward_http(body, &session, headers, &mut stream)
        }
        _ => {
            let headers = auth_headers(secret, &request_id, true)?;
            forward_ws(body, &session, headers, &mut stream)
        }
    }
}

fn forward_ws(body: Value, session: &str, headers: Value, stream: &mut Stream<'_>) -> Result<()> {
    let outbound = request(&body, session, true)?;
    let socket = WebSocket::open(
        "wss://chatgpt.com/backend-api/codex/responses",
        headers,
        &[],
    )?;
    socket.send_text(&outbound.to_string())?;
    loop {
        let frame = socket.read()?;
        if frame["type"] != "text" {
            return Err(Error::http(502, "Halo Codex 在完成前关闭").with_code("codex_ws_closed"));
        }
        let event: Value = serde_json::from_str(string(&frame["data"]))
            .map_err(|_| Error::http(502, "Halo Codex 返回无效事件"))?;
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
            class["message"].as_str().unwrap_or("Codex HTTP 请求失败"),
        )
        .with_code("codex_http_error")
        .with_reset(class["reset_at"].as_i64()));
    }

    let mut decoder = SseDecoder::default();
    while let Some(bytes) = response.read()? {
        for data in decoder.push(&bytes)? {
            if data == "[DONE]" {
                return Err(
                    Error::http(502, "Codex HTTP 流提前中断").with_code("codex_http_closed")
                );
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
    fn upstream_failure_after_creation_does_not_start_the_stream() {
        let mut stream = Stream {
            model: "test-model",
            id: "test-id".into(),
            tools: BTreeMap::new(),
            started: false,
        };
        assert!(!stream.handle(&json!({"type":"response.created"})).unwrap());
        let error = stream
            .handle(&json!({
                "type":"response.failed",
                "response":{"error":{"code":"rate_limit_exceeded","message":"Rate limit","retry_after":30}}
            }))
            .unwrap_err();
        assert_eq!(error.status, Some(429));
        assert_eq!(error.reset_at, Some(sdk::now() + 30_000));
        assert!(!stream.started);
    }

    #[test]
    fn normalizes_native_shorthand_without_losing_instruction_roles() {
        let (instructions, items) =
            input(&json!({"instructions":"brief","input":"hello"})).unwrap();
        assert_eq!(instructions, "brief");
        assert_eq!(items[0]["content"][0]["text"], "hello");
        let (_, items) = input(&json!({"input":[{"role":"system","content":"be brief"}]})).unwrap();
        assert_eq!(items[0]["role"], "developer");
        assert_eq!(items[0]["content"][0]["text"], "be brief");

        let custom = json!({"type":"custom","name":"lookup"});
        assert_eq!(convert_tool_choice(&custom), custom);
        assert_eq!(
            convert_tool_choice(&json!({"type":"function","function":{"name":"lookup"}})),
            json!({"type":"function","name":"lookup"})
        );
    }

    #[test]
    fn request_reasoning_prefers_body_then_default() {
        let mut output = json!({});
        apply_reasoning(
            &json!({"reasoning_effort": "high"}),
            Some("medium"),
            &mut output,
        );
        assert_eq!(output["reasoning"]["effort"], "high");

        let mut output = json!({});
        apply_reasoning(
            &json!({"reasoning": {"effort": "low"}}),
            Some("medium"),
            &mut output,
        );
        assert_eq!(output["reasoning"]["effort"], "low");

        let mut output = json!({});
        apply_reasoning(&json!({}), Some("medium"), &mut output);
        assert_eq!(output["reasoning"]["effort"], "medium");

        let mut output = json!({});
        apply_reasoning(&json!({}), None, &mut output);
        assert!(output.get("reasoning").is_none());

        apply_reasoning(&json!({"reasoning_effort": "ultra"}), None, &mut output);
        assert_eq!(output["reasoning"]["effort"], "max");
    }

    #[test]
    fn converts_openai_and_native_tools() {
        let tools = convert_tools(&[
            json!({"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}),
            json!({"type":"web_search","external_web_access":false}),
            json!({"type":"custom","name":"shell.exec","description":"run"}),
        ])
        .unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "lookup");
        assert_eq!(tools[1]["type"], "web_search");
        assert_eq!(tools[2]["type"], "custom");
        assert!(convert_tools(&[json!({"type":"function","function":{"name":"坏名"}})]).is_err());
    }

    #[test]
    fn accepts_native_input_and_tool_results() {
        let (instructions, items) = input(&json!({
            "instructions": "be brief",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"ok"}
            ]
        }))
        .unwrap();
        assert_eq!(instructions, "be brief");
        assert_eq!(items.len(), 3);
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[2]["type"], "function_call_output");

        let (_, chat_items) = input_messages(
            &[
                json!({"role":"assistant","content":"","tool_calls":[
                    {"id":"c2","type":"function","function":{"name":"lookup","arguments":"{}"}},
                    {"id":"c3","type":"custom","custom":{"name":"shell.exec","input":"ls"}}
                ]}),
                json!({"role":"tool","tool_call_id":"c2","content":"done"}),
            ],
            true,
        )
        .unwrap();
        assert_eq!(chat_items[0]["type"], "function_call");
        assert_eq!(chat_items[1]["type"], "custom_tool_call");
        assert_eq!(chat_items[2]["type"], "function_call_output");
    }

    #[test]
    fn drops_plaintext_reasoning_keeps_encrypted_and_declares_search() {
        let err = reject_stateless_history(&json!({"previous_response_id":"resp_1"})).unwrap_err();
        assert_eq!(
            err.code.as_deref(),
            Some("codex_stateless_history_required")
        );

        let (_, items) = input(&json!({
            "input": [
                {"type":"reasoning","content":[{"type":"summary_text","text":"think"}]},
                {"type":"reasoning","encrypted_content":"enc-1"},
                {"type":"web_search_call","id":"ws1"},
                {"type":"message","role":"system","content":[{"type":"input_text","text":"x"}]}
            ]
        }))
        .unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[0]["encrypted_content"], "enc-1");
        assert!(items[0].get("content").is_none());
        assert_eq!(items[2]["role"], "developer");

        let mut output = json!({
            "input": items,
            "reasoning": {"effort": "medium"}
        });
        ensure_encrypted_include(&mut output);
        declare_search(&mut output);
        assert_eq!(output["include"][0], "reasoning.encrypted_content");
        assert_eq!(output["reasoning"]["summary"], "auto");
        assert_eq!(output["tools"][0]["type"], "web_search");
    }
}
