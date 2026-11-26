mod auth;
mod chat;
mod config;
mod errors;
mod images;
mod models;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

const PROVIDER: &str = "halo-codex";

sdk::export_provider!(handle);

fn login_methods() -> Value {
    json!({"methods": [
      {"methodIndex": 0, "type": "oauth", "label": "Codex 设备授权", "browserMode":"external","prompts":[]},
      {
        "methodIndex": 1, "type": "api", "label": "导入 Codex 访问凭据",
        "prompts": [{"key": "accessToken", "label": "Codex access token", "type": "text", "secret": true}]
    }]})
}

fn metadata() -> Value {
    let meta = json!({
        "id": PROVIDER, "label": "Halo Codex", "order": 46,
        "capabilities": {"chat": true, "stateful": true, "supports_default_model": false,
            "web_login":true,"refresh":true,"refresh_uses_account":true,"usage":true,
            "model_refresh":true,"images":true},
        "account_features": {
            "usage": true,
            "identifier": ["accountId"],
            "expiry": ["expiresAt", "tokenExpiresAt"]
        },
        "default_models": models::fallback(),
        "settings_schema": config::schema(),
        "settings_group": { "label": "Halo Codex" }
    });
    sdk::error_rules::attach(meta, vec![], vec![])
}

fn normalize_access(access: &str) -> Result<Value> {
    let claims = access
        .split('.')
        .nth(1)
        .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
        .ok_or_else(|| Error::http(400, "Codex 访问凭据格式无效"))?;
    let account = claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::http(400, "Codex 凭据缺少账号标识"))?;
    let expires = claims["exp"].as_i64().unwrap_or(0).saturating_mul(1000);
    if expires <= sdk::now() {
        return Err(Error::http(401, "Codex 登录凭据已过期"));
    }
    let user = claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_user_id")
        .and_then(Value::as_str)
        .or_else(|| claims["sub"].as_str())
        .unwrap_or(account);
    Ok(
        json!({"accessToken": access, "accountId": account, "expiresAt": expires,"identity":format!("{account}:{user}")}),
    )
}

fn public(secret: &Value) -> Value {
    json!({
        "name": "Halo Codex",
        "accountId": secret["accountId"],
        "expiresAt": secret["expiresAt"],
        "tokenExpiresAt": secret["expiresAt"]
    })
}

fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let payload = &source["payload"];
    let access = payload
        .pointer("/inputs/accessToken")
        .or_else(|| payload.get("accessToken"))
        .or_else(|| payload.get("token"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::http(400, "需要 Codex access token"))?;
    let secret = normalize_access(access)?;
    Ok(normalized(secret))
}

fn normalized(secret: Value) -> Value {
    json!({"identity":secret["identity"],"public":public(&secret),"secret":secret,"desktop":false})
}

fn status(params: &Value) -> u16 {
    params["status"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(502)
}

fn admin(params: &Value) -> Result<Value> {
    let payload = &params["payload"];
    match params["action"].as_str().unwrap_or("") {
        "auth.methods" => Ok(login_methods()),
        "quota.sources" => Ok(usage::sources(payload.get("report").unwrap_or(payload))),
        "network.user-agent" => Ok(json!({ "accountWide": false })),
        _ => Err(Error::unsupported()),
    }
}

fn handle(method: &str, mut params: Value) -> Result<Value> {
    match method {
        "init" => Ok(json!([metadata()])),
        "ping" => Ok(json!(true)),
        "provider.info" => Ok(metadata()),
        "account.normalize" => normalize(&params),
        "account.view" => Ok(public(&params["secret"])),
        "account.identity" => Ok(params["secret"]["identity"].clone()),
        "models.list" => models::list(&params),
        "models.refresh" => models::refresh(&params),
        "admin.action" => admin(&params),
        "usage.query" => usage::query(),
        "conversation" => {
            let account = auth::credentials(false)?;
            let body =
                sdk::take_body(&mut params).ok_or_else(|| Error::http(400, "请求缺少正文"))?;
            chat::forward(body, &params, &account)?;
            Ok(Value::Null)
        }
        "token.ensure" => Ok(auth::credentials(false)?["accessToken"].clone()),
        "token.refresh" => Ok(auth::credentials(true)?["accessToken"].clone()),
        "token.expiring" => Ok(json!(auth::expiring(&params)?)),
        "login.start" => auth::start(&params),
        "login.poll" => auth::poll(),
        "login.cancel" => auth::cancel(),
        "reasoning_patch" => Ok(models::reasoning_patch(&params)),
        "is_error_response" => Ok(json!(errors::is_error_response(&params))),
        "refusal_markers" => Ok(json!({ "finishes": ["content_filter"], "markers": [] })),
        "cooldown_models" => Ok(json!({ "models": [] })),
        "classify" => Ok(errors::classify(status(&params), &params["body"])),
        "error.policy" => Ok(errors::policy(status(&params), &params["body"])),
        "retry.decision" => Ok(errors::retry(&params)),
        "retry_advice" => Ok(errors::advice(&params)),
        "images.capabilities" => Ok(json!({ "generation": true, "editing": true })),
        "images.generate" => images::generate(&params, false),
        "images.edit" => images::generate(&params, true),
        _ => Err(Error::unsupported()),
    }
}
