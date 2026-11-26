use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

pub const TRANSPORT_WS: &str = "websocket";
pub const TRANSPORT_HTTP: &str = "http-sse";
/// Matches the user's current local Codex models cache (`~/.codex/models_cache.json`).
pub const DEFAULT_CLIENT_VERSION: &str = "0.171.0";
/// Catalog / usage identity; chat WebSocket historically used `pi` and stays that way unless overridden.
pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";
pub const DEFAULT_CHAT_ORIGINATOR: &str = "pi";
pub const REASONING_AUTO: &str = "auto";

pub fn schema() -> Value {
    json!({
        "version": 1,
        "title": "Codex WebSocket 设置",
        "description": "控制对话转发、客户端身份与默认推理强度；模型列表在登录后从 Codex 目录读取。",
        "runtime_overrides": false,
        "fields": [
            {
                "key": "transport",
                "label": "转发方式",
                "type": "select",
                "default": TRANSPORT_WS,
                "description": "WebSocket 为默认长连接；HTTP SSE 走 POST /codex/responses 流式接口。",
                "options": [
                    { "value": TRANSPORT_WS, "label": "WebSocket" },
                    { "value": TRANSPORT_HTTP, "label": "HTTP SSE" }
                ]
            },
            {
                "key": "clientVersion",
                "label": "Codex 客户端版本",
                "type": "string",
                "default": DEFAULT_CLIENT_VERSION,
                "trim": true,
                "empty_is_default": true,
                "max_length": 32,
                "pattern": "^[0-9]+(\\.[0-9]+){1,3}([.-][0-9A-Za-z]+)*$",
                "description": "用于 GET /codex/models 的 client_version，请与本机 Codex 客户端一致。"
            },
            {
                "key": "originator",
                "label": "Originator",
                "type": "string",
                "default": DEFAULT_ORIGINATOR,
                "trim": true,
                "empty_is_default": true,
                "max_length": 64,
                "description": "上游 originator。目录/用量默认 codex_cli_rs；对话默认仍用 pi（可在此覆盖）。"
            },
            {
                "key": "reasoningEffort",
                "label": "默认推理强度",
                "type": "select",
                "default": REASONING_AUTO,
                "description": "请求未带 reasoning / reasoning_effort 时使用；auto 表示不强制覆盖。",
                "options": [
                    { "value": REASONING_AUTO, "label": "跟随请求（auto）" },
                    { "value": "minimal", "label": "minimal" },
                    { "value": "low", "label": "low" },
                    { "value": "medium", "label": "medium" },
                    { "value": "high", "label": "high" },
                    { "value": "xhigh", "label": "xhigh" }
                ]
            }
        ]
    })
}

pub fn transport() -> Result<String> {
    let value = sdk::settings::get("transport")?;
    Ok(match value.as_str().map(str::trim).unwrap_or("") {
        TRANSPORT_HTTP => TRANSPORT_HTTP.into(),
        _ => TRANSPORT_WS.into(),
    })
}

pub fn client_version() -> Result<String> {
    Ok(sanitize_version(
        sdk::settings::get("clientVersion")?
            .as_str()
            .map(str::trim)
            .unwrap_or(""),
    ))
}

pub fn originator(for_chat: bool) -> Result<String> {
    let value = sdk::settings::get("originator")?;
    let trimmed = value.as_str().map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        return Ok(if for_chat {
            DEFAULT_CHAT_ORIGINATOR.into()
        } else {
            DEFAULT_ORIGINATOR.into()
        });
    }
    Ok(sanitize_token(trimmed, if for_chat {
        DEFAULT_CHAT_ORIGINATOR
    } else {
        DEFAULT_ORIGINATOR
    }))
}

pub fn reasoning_effort() -> Result<Option<String>> {
    let value = sdk::settings::get("reasoningEffort")?;
    Ok(match value.as_str().map(str::trim).unwrap_or("") {
        "" | REASONING_AUTO => None,
        effort if matches!(effort, "minimal" | "low" | "medium" | "high" | "xhigh") => {
            Some(effort.into())
        }
        _ => None,
    })
}

fn sanitize_version(trimmed: &str) -> String {
    if trimmed.is_empty()
        || trimmed.len() > 32
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        || !trimmed.as_bytes()[0].is_ascii_digit()
    {
        DEFAULT_CLIENT_VERSION.into()
    } else {
        trimmed.to_string()
    }
}

fn sanitize_token(trimmed: &str, fallback: &str) -> String {
    if trimmed.is_empty()
        || trimmed.len() > 64
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        fallback.into()
    } else {
        trimmed.to_string()
    }
}
