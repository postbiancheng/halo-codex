mod auth;
mod chat;
mod config;
mod errors;
mod images;
mod models;
mod usage;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
    Engine,
};

const PROVIDER: &str = "halo-codex";

sdk::export_provider!(handle);

fn login_methods() -> Value {
    json!({"methods": [
      {"methodIndex": 0, "type": "oauth", "label": "Codex 设备授权", "browserMode":"external","prompts":[]},
      {
        "methodIndex": 1, "type": "api", "label": "导入 Codex 凭据",
        "prompts": [
          {"key": "accessToken", "label": "access token 或完整 auth.json", "type": "text", "secret": true},
          {"key": "refreshToken", "label": "refresh token（可选，可续期）", "type": "text", "secret": true, "optional": true}
        ]
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
            "credit_detail": true,
            "credit_expiry": true,
            "identifier": ["accountId", "email", "loginName"],
            "expiry": ["expiresAt", "tokenExpiresAt"]
        },
        "default_models": models::fallback(),
        "settings_schema": config::schema(),
        "settings_group": { "label": "Halo Codex" }
    });
    sdk::error_rules::attach(meta, vec![], vec![])
}

const AUTH_CLAIMS: &str = "https://api.openai.com/auth";
const PROFILE_CLAIMS: &str = "https://api.openai.com/profile";

fn jwt_claims(token: &str) -> Option<Value> {
    let mut segments = token.split('.');
    let header = segments.next()?;
    let encoded = segments.next()?;
    let signature = segments.next()?;
    if header.is_empty() || encoded.is_empty() || signature.is_empty() || segments.next().is_some()
    {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .or_else(|_| URL_SAFE.decode(encoded))
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.is_object().then_some(claims)
}

fn auth_claims(claims: &Value) -> Option<&Value> {
    claims.get(AUTH_CLAIMS).filter(|value| value.is_object())
}

fn profile_claims(claims: &Value) -> Option<&Value> {
    claims.get(PROFILE_CLAIMS).filter(|value| value.is_object())
}

fn claim<'a>(claims: &'a Value, key: &str) -> Option<&'a str> {
    auth_claims(claims)
        .and_then(|auth| auth.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn display_name(claims: &Value) -> &str {
    [
        profile_claims(claims)
            .and_then(|profile| profile.get("email"))
            .and_then(Value::as_str),
        profile_claims(claims)
            .and_then(|profile| profile.get("name"))
            .and_then(Value::as_str),
        claims.get("email").and_then(Value::as_str),
        claims.get("preferred_username").and_then(Value::as_str),
        claim(claims, "user_email"),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())
    .unwrap_or("Halo Codex")
}

fn profile_email(claims: &Value) -> Option<&str> {
    profile_claims(claims)
        .and_then(|profile| profile.get("email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn normalize_access(access: &str) -> Result<Value> {
    normalize_tokens(access, None, false)
}

fn expired(expires: i64, refresh: Option<&str>, allow_expired: bool, now: i64) -> bool {
    expires <= now && !(allow_expired && refresh.is_some())
}

fn normalize_tokens(access: &str, refresh: Option<&str>, allow_expired: bool) -> Result<Value> {
    let access = access.trim();
    if access.is_empty() || access.starts_with("sk-") {
        return Err(Error::http(
            400,
            "请导入 Codex 的 ChatGPT 登录凭据；API Key 不能用于订阅通道",
        ));
    }
    let claims = jwt_claims(access).ok_or_else(|| Error::http(400, "Codex 访问凭据格式无效"))?;
    let account = claim(&claims, "chatgpt_account_id")
        .ok_or_else(|| Error::http(400, "Codex 凭据缺少账号标识"))?;
    let expires = claims["exp"]
        .as_i64()
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::http(400, "Codex 凭据缺少有效过期时间"))?
        .saturating_mul(1000);
    let refresh = refresh.map(str::trim).filter(|value| !value.is_empty());
    if expired(expires, refresh, allow_expired, sdk::now()) {
        return Err(Error::http(401, "Codex 登录凭据已过期"));
    }
    let user = claim(&claims, "chatgpt_user_id")
        .or_else(|| claims["sub"].as_str())
        .unwrap_or(account);
    let name = display_name(&claims);
    let mut secret = json!({
        "accessToken": access,
        "accountId": account,
        "expiresAt": expires,
        "identity": format!("{account}:{user}"),
        "displayName": name
    });
    if let Some(email) = profile_email(&claims) {
        secret["email"] = json!(email);
        secret["loginName"] = json!(email);
    }
    if let Some(token) = refresh {
        if token.len() < 20 {
            return Err(Error::http(400, "Codex refresh token 格式无效"));
        }
        secret["refreshToken"] = json!(token);
    }
    Ok(secret)
}

fn credential_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_auth_blob(raw: &str) -> Option<(String, Option<String>)> {
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let parsed: Value = serde_json::from_str(trimmed).ok()?;
    let root = parsed
        .get("secret")
        .or_else(|| parsed.get("pluginSecret"))
        .filter(|value| value.is_object())
        .unwrap_or(&parsed);
    let tokens = root
        .get("tokens")
        .or_else(|| root.get("auth"))
        .filter(|value| value.is_object())
        .unwrap_or(root);
    let mode = credential_field(root, &["auth_mode", "authMode"]).unwrap_or("chatgpt");
    if mode != "chatgpt" {
        return None;
    }
    let access = credential_field(tokens, &["accessToken", "access_token"])?.to_string();
    let refresh = credential_field(tokens, &["refreshToken", "refresh_token"]).map(str::to_owned);
    Some((access, refresh))
}

fn public(secret: &Value) -> Value {
    let claims = secret["accessToken"]
        .as_str()
        .and_then(jwt_claims)
        .unwrap_or(Value::Null);
    let email = secret["email"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| profile_email(&claims));
    let name = secret["displayName"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "Halo Codex")
        .or(email)
        .or_else(|| {
            let derived = display_name(&claims);
            (derived != "Halo Codex").then_some(derived)
        })
        .unwrap_or("Halo Codex");
    let mut view = json!({
        "name": name,
        "accountId": secret["accountId"],
        "expiresAt": secret["expiresAt"],
        "tokenExpiresAt": secret["expiresAt"]
    });
    if let Some(email) = email {
        view["email"] = json!(email);
        view["loginName"] = json!(email);
    }
    view
}

fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let payload = &source["payload"];
    let inputs = payload.get("inputs").unwrap_or(payload);
    let raw = inputs
        .get("accessToken")
        .or_else(|| payload.get("accessToken"))
        .or_else(|| payload.get("token"))
        .or_else(|| payload.get("authJson"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::http(400, "需要 Codex access token 或完整 auth.json"))?;
    let prompt_refresh = inputs
        .get("refreshToken")
        .or_else(|| payload.get("refreshToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (access, embedded_refresh) = match parse_auth_blob(raw) {
        Some((access, refresh)) => (access, refresh),
        None => (raw.trim().to_string(), None),
    };
    let refresh = prompt_refresh.or(embedded_refresh.as_deref());
    let secret = normalize_tokens(&access, refresh, refresh.is_some())?;
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
        "cooldown_models" => Ok(models::cooldown_models(&params)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn fake_access(claims: Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("hdr.{payload}.sig")
    }

    #[test]
    fn parses_auth_json_blob() {
        let raw = r#"{
            "auth_mode":"chatgpt",
            "tokens":{
                "access_token":"aaa.bbb.ccc",
                "refresh_token":"refresh-token-value-1234567890"
            }
        }"#;
        let (access, refresh) = parse_auth_blob(raw).unwrap();
        assert_eq!(access, "aaa.bbb.ccc");
        assert_eq!(refresh.as_deref(), Some("refresh-token-value-1234567890"));
        assert!(parse_auth_blob("plain-token").is_none());
    }

    #[test]
    fn rejects_malformed_jwt_shapes_and_missing_expiry() {
        let exp = sdk::now() / 1000 + 3600;
        let claims = json!({
            "exp": exp,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}
        });
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        for token in [
            format!("{payload}.sig"),
            format!("hdr.{payload}"),
            format!("hdr.{payload}.sig.extra"),
            format!(".{payload}.sig"),
            format!("hdr.{payload}."),
        ] {
            assert!(normalize_access(&token).is_err(), "accepted {token}");
        }
        assert!(normalize_access(&format!("hdr.{payload}.sig")).is_ok());
        assert!(normalize_access("hdr.%%%.sig").is_err());
        assert!(normalize_access(&fake_access(json!([]))).is_err());
        let missing_account = fake_access(json!({"exp": exp}));
        assert_eq!(
            normalize_access(&missing_account).unwrap_err().status,
            Some(400)
        );

        let missing_exp = fake_access(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}
        }));
        let error = normalize_tokens(&missing_exp, Some("refresh-token-value-1234567890"), true)
            .unwrap_err();
        assert_eq!(error.status, Some(400));
    }

    #[test]
    fn handles_expiry_and_auth_json_variants_without_exposing_tokens() {
        let refresh = "refresh-token-value-1234567890";
        assert!(expired(1000, None, false, 1000));
        assert!(!expired(1000, Some(refresh), true, 1000));
        assert!(!expired(1001, None, false, 1000));

        let future = fake_access(json!({
            "exp": sdk::now() / 1000 + 3600,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-1",
                "chatgpt_user_id": "user-1"
            }
        }));
        for raw in [
            json!({"tokens": {"access_token": future, "refresh_token": refresh}}),
            json!({"secret": {"auth": {"accessToken": future, "refreshToken": refresh}}}),
            json!({"pluginSecret": {"tokens": {"access_token": future}}}),
        ] {
            let (access, _) = parse_auth_blob(&raw.to_string()).unwrap();
            assert_eq!(access, future);
        }
        assert!(parse_auth_blob(
            &json!({
                "auth_mode": "apikey",
                "tokens": {"access_token": future}
            })
            .to_string()
        )
        .is_none());
        assert!(parse_auth_blob("{not-json").is_none());

        let account = normalize(&json!({
            "source": {"payload": {"inputs": {
                "accessToken": future,
                "refreshToken": refresh
            }}}
        }))
        .unwrap();
        let serialized = account["public"].to_string();
        assert!(!serialized.contains(&future));
        assert!(!serialized.contains(refresh));
        assert!(account["public"].get("accessToken").is_none());
        assert!(account["public"].get("refreshToken").is_none());
        assert!(account["public"].get("secret").is_none());
    }

    #[test]
    fn reads_nested_auth_and_profile_claims() {
        let exp = sdk::now() / 1000 + 3600;
        let access = fake_access(json!({
            "exp": exp,
            "sub": "google-oauth2|1",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-1",
                "chatgpt_user_id": "user-1"
            },
            "https://api.openai.com/profile": {
                "email": "demo@example.com",
                "name": "Demo User"
            }
        }));
        let secret =
            normalize_tokens(&access, Some("refresh-token-value-1234567890"), false).unwrap();
        assert_eq!(secret["accountId"], "acct-1");
        assert_eq!(secret["displayName"], "demo@example.com");
        assert_eq!(secret["email"], "demo@example.com");
        let view = public(&json!({
            "accessToken": access,
            "accountId": "acct-1",
            "expiresAt": exp * 1000
        }));
        assert_eq!(view["name"], "demo@example.com");
        assert_eq!(view["email"], "demo@example.com");
        assert_eq!(view["loginName"], "demo@example.com");
    }
}
