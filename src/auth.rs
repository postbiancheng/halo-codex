use agent2api_wasm_sdk::{
    self as sdk, http, json, login,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

const CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN: &str = "https://auth.openai.com/oauth/token";
const KEY: &str = "codex-device";
const TTL: i64 = 900_000;
const LEAD: i64 = 300_000;

fn put(snapshot: &state::Snapshot, value: Value, ttl: i64) -> Result<()> {
    if state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, value, ttl)? {
        Ok(())
    } else {
        Err(Error::http(409, "Codex 授权状态已变化，请重新发起"))
    }
}

fn post_json(url: &str, value: Value) -> Result<(u16, Value)> {
    let response = http::open(
        "POST",
        url,
        json!({"Content-Type":"application/json","Accept":"application/json"}),
        Some(&value),
    )?;
    let status = response.status;
    if (200..300).contains(&status) {
        Ok((status, response.json()?))
    } else {
        Ok((status, Value::Null))
    }
}

pub fn start(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .ok_or_else(|| Error::http(400, "缺少授权流程标识"))?;
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.value.is_null() {
        return Err(Error::http(409, "Codex 授权流程已存在"));
    }
    let (status, reply) = post_json(
        "https://auth.openai.com/api/accounts/deviceauth/usercode",
        json!({"client_id":CLIENT}),
    )?;
    if !(200..300).contains(&status) {
        return Err(Error::http(status, "获取 Codex 设备验证码失败"));
    }
    let device = reply["device_auth_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::http(502, "授权响应缺少设备标识"))?;
    let code = reply
        .get("user_code")
        .or_else(|| reply.get("usercode"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty() && v.len() <= 64 && !v.chars().any(char::is_control))
        .ok_or_else(|| Error::http(502, "授权响应缺少验证码"))?;
    let interval = reply["interval"]
        .as_i64()
        .or_else(|| reply["interval"].as_str()?.parse().ok())
        .unwrap_or(5)
        .clamp(1, 60)
        * 1000;
    let now = sdk::now();
    put(
        &before,
        json!({"device":device,"code":code,"interval":interval,"nextAt":now+interval,"expiresAt":now+TTL,"busy":false}),
        TTL,
    )?;
    Ok(
        json!({"flow":flow,"url":"https://auth.openai.com/codex/device","poll_interval_ms":interval,
        "instructions":format!("打开 OpenAI 官方页面并输入验证码：{code}"),
        "external_browser":true,"allowed_hosts":["auth.openai.com","chatgpt.com"],
        "login_ui":{"browserMode":"external","manualCallback":false,"callbackInput":"url","captureCallback":false}}),
    )
}

pub fn poll() -> Result<Value> {
    let before = state::get(Scope::Login, KEY, 1)?;
    let now = sdk::now();
    let deadline = before.value["expiresAt"].as_i64().unwrap_or(0);
    if before.value.is_null() || deadline <= now {
        return Ok(json!({"state":"failed","message":"Codex 设备验证码已过期"}));
    }
    if before.value["busy"] == true || before.value["nextAt"].as_i64().unwrap_or(0) > now {
        return Ok(login::pending(None));
    }
    let mut flow = before.value.clone();
    flow["busy"] = json!(true);
    put(&before, flow.clone(), deadline - now)?;
    let reserved = state::get(Scope::Login, KEY, 1)?;
    if reserved.value != flow {
        return Err(Error::http(409, "Codex 授权已取消"));
    }
    let exchange = post_json(
        "https://auth.openai.com/api/accounts/deviceauth/token",
        json!({"device_auth_id":flow["device"],"user_code":flow["code"]}),
    );
    let (status, reply) = match exchange {
        Ok(result) => result,
        Err(_) => (0, Value::Null),
    };
    if matches!(status, 0 | 403 | 404) {
        flow["busy"] = json!(false);
        flow["nextAt"] = json!(sdk::now() + flow["interval"].as_i64().unwrap_or(5000));
        put(&reserved, flow, (deadline - sdk::now()).max(1000))?;
        return Ok(login::pending(None));
    }
    put(&reserved, Value::Null, 1000)?;
    if !(200..300).contains(&status) {
        return Err(Error::http(status, "查询 Codex 授权失败"));
    }
    let code = reply["authorization_code"]
        .as_str()
        .ok_or_else(|| Error::http(502, "Codex 授权响应缺少授权码"))?;
    let verifier = reply["code_verifier"]
        .as_str()
        .ok_or_else(|| Error::http(502, "Codex 授权响应缺少校验值"))?;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", CLIENT)
        .append_pair("code", code)
        .append_pair("code_verifier", verifier)
        .append_pair(
            "redirect_uri",
            "https://auth.openai.com/deviceauth/callback",
        )
        .finish();
    let response = http::open_raw(
        "POST",
        TOKEN,
        json!({"Content-Type":"application/x-www-form-urlencoded"}),
        Some(form.as_bytes()),
    )?;
    if !(200..300).contains(&response.status) {
        return Err(Error::http(response.status, "换取 Codex 登录凭据失败"));
    }
    let tokens = response.json()?;
    Ok(login::done(super::normalized(from_tokens(&tokens, None)?)))
}

pub fn cancel() -> Result<Value> {
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    put(&snapshot, Value::Null, 1000)?;
    Ok(Value::Null)
}

fn from_tokens(tokens: &Value, old: Option<&Value>) -> Result<Value> {
    let access = tokens["access_token"]
        .as_str()
        .ok_or_else(|| Error::http(502, "Codex 未返回访问凭据"))?;
    let mut secret = super::normalize_access(access)?;
    let refresh = tokens["refresh_token"]
        .as_str()
        .or_else(|| old?.get("refreshToken")?.as_str())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::http(502, "Codex 未返回续期凭据"))?;
    secret["refreshToken"] = json!(refresh);
    Ok(secret)
}

fn due(secret: &Value, lead: i64) -> bool {
    secret["expiresAt"].as_i64().unwrap_or(0) <= sdk::now().saturating_add(lead)
}

pub fn expiring(params: &Value) -> Result<bool> {
    let account = sdk::account()?;
    let secret = &account["secret"];
    Ok(secret["refreshToken"].is_string() && due(secret, sdk::expiring_lead(params, LEAD)))
}

pub fn credentials(force: bool) -> Result<Value> {
    let mut initial = sdk::account()?;
    let secret = initial
        .as_object_mut()
        .and_then(|record| record.remove("secret"))
        .ok_or_else(|| Error::http(401, "Codex 未登录"))?;
    if !force && (!due(&secret, LEAD) || !secret["refreshToken"].is_string()) {
        return Ok(secret);
    }
    let _lock = AccountLock::acquire()?;
    let account = sdk::account()?;
    let current = &account["secret"];
    if current["accessToken"] != secret["accessToken"] || !force && !due(current, LEAD) {
        return Ok(current.clone());
    }
    let token = current["refreshToken"]
        .as_str()
        .ok_or_else(|| Error::http(401, "请重新授权 Codex"))?;
    let (status, tokens) = post_json(
        TOKEN,
        json!({"grant_type":"refresh_token","client_id":CLIENT,"refresh_token":token}),
    )?;
    if !(200..300).contains(&status) {
        return Err(Error::http(status, "Codex 登录续期失败，请重新授权"));
    }
    let new = from_tokens(&tokens, Some(current))?;
    if new["identity"] != current["identity"] {
        return Err(Error::http(409, "Codex 账号身份已变化"));
    }
    let saved = sdk::host(
        "account.update",
        json!({"expect":current,"secret":new,"public":super::public(&new)}),
    )?;
    if saved["updated"] != true {
        return Err(Error::http(409, "保存 Codex 续期凭据失败"));
    }
    Ok(new)
}
