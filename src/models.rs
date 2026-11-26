//! Remote Codex model catalog with local fallback.

use super::{auth, config, images, PROVIDER};
use agent2api_wasm_sdk::{
    self as sdk, http, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "model-catalog";
const TTL: i64 = 600_000;
const FALLBACK: &[(&str, &str, i64)] = &[
    ("gpt-6-luna", "gpt-6-luna", 272_000),
    ("gpt-6.2-sol", "gpt-6.2-sol", 500_000),
    ("gpt-6.2-astra", "gpt-6.2-astra", 272_000),
];

pub fn fallback() -> Value {
    let mut models: Vec<Value> = FALLBACK
        .iter()
        .map(|(id, name, limit)| model(id, name, *limit))
        .collect();
    models.extend(images::catalog_rows());
    Value::Array(models)
}

fn with_image_models(mut models: Vec<Value>) -> Vec<Value> {
    let existing: std::collections::BTreeSet<String> = models
        .iter()
        .filter_map(|row| row["id"].as_str().map(str::to_owned))
        .collect();
    for row in images::catalog_rows() {
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        if !existing.contains(id) {
            models.push(row);
        }
    }
    models
}

fn model(id: &str, name: &str, limit: i64) -> Value {
    json!({
        "id": id,
        "name": name,
        "kind": "chat",
        "provider": PROVIDER,
        "capabilities": {
            "maxInputTokens": limit,
            "maxOutputTokens": 128000,
            "supportsTools": true
        }
    })
}

fn catalog(models: Value, remote: bool, refreshed_at: i64) -> Value {
    catalog_with_profiles(models, json!({}), remote, refreshed_at)
}

fn catalog_with_profiles(
    models: Value,
    profiles: Value,
    remote: bool,
    refreshed_at: i64,
) -> Value {
    json!({
        "models": models,
        "meta": {
            "remote": remote,
            "refreshed_at": refreshed_at,
            "profiles": profiles
        }
    })
}

/// Private Codex catalog profile used when building upstream requests.
pub fn profile_for(model: &str) -> Option<Value> {
    if model.is_empty() {
        return None;
    }
    let catalog = cached_for(&sdk::account().ok()?["secret"]).or_else(|| {
        sdk::catalog::cached()
            .ok()
            .filter(|value| value["models"].is_array())
    })?;
    catalog
        .pointer("/meta/profiles")?
        .as_object()?
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(model))
        .map(|(_, value)| value.clone())
}

fn load() -> Result<state::Snapshot> {
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "Codex 模型目录状态格式已变化"));
    }
    Ok(snapshot)
}

fn identity(secret: &Value) -> String {
    secret["identity"].as_str().unwrap_or_default().to_string()
}

fn cached_for(secret: &Value) -> Option<Value> {
    let snapshot = load().ok()?;
    if snapshot.value["source"] != identity(secret) {
        return None;
    }
    let catalog = snapshot.value.get("catalog")?;
    catalog["models"].as_array()?;
    Some(ensure_images(catalog.clone()))
}

fn ensure_images(mut catalog: Value) -> Value {
    let models = catalog["models"].as_array().cloned().unwrap_or_default();
    catalog["models"] = Value::Array(with_image_models(models));
    catalog
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        if let Ok(cached) = sdk::catalog::cached() {
            if cached["models"].is_array() && cached["meta"]["remote"] == true {
                return Ok(ensure_images(cached));
            }
        }
        return Ok(catalog(fallback(), false, 0));
    }
    let account = sdk::account()?;
    if let Some(cached) = cached_for(&account["secret"]) {
        return Ok(cached);
    }
    Ok(catalog(fallback(), false, 0))
}

pub fn refresh(params: &Value) -> Result<Value> {
    let credentials = match auth::credentials(false) {
        Ok(value) => value,
        Err(error) => {
            return Ok(json!({
                "refreshed": false,
                "message": error.message,
                "count": fallback().as_array().map(Vec::len).unwrap_or(0)
            }));
        }
    };
    let previous = cached_for(&credentials).unwrap_or_else(|| catalog(fallback(), false, 0));
    let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if params["force"] != true && previous["meta"]["remote"] == true && (0..TTL).contains(&age) {
        return Ok(sdk::catalog::unchanged(
            previous["models"].as_array().map(Vec::len).unwrap_or(0),
        ));
    }

    let access = credentials["accessToken"]
        .as_str()
        .ok_or_else(|| Error::http(401, "请重新授权 Codex"))?;
    let account_id = credentials["accountId"]
        .as_str()
        .ok_or_else(|| Error::http(401, "Codex 凭据缺少账号标识"))?;
    let client_version = config::client_version()?;
    let originator = config::originator(false)?;
    let url = format!(
        "https://chatgpt.com/backend-api/codex/models?client_version={client_version}"
    );
    let report = match http::open(
        "GET",
        &url,
        json!({
            "Authorization": format!("Bearer {access}"),
            "ChatGPT-Account-Id": account_id,
            "originator": originator,
            "User-Agent": config::user_agent(false)?,
            "version": client_version,
            "Accept": "application/json",
        }),
        None,
    )
    .and_then(|response| response.json())
    {
        Ok(value) => value,
        Err(error) => {
            return Ok(json!({
                "refreshed": false,
                "message": error.message,
                "count": previous["models"].as_array().map(Vec::len).unwrap_or(0),
                "models": previous["models"]
            }));
        }
    };

    let (models, profiles) = match parse(&report) {
        Ok((models, profiles)) if !models.is_empty() => (models, profiles),
        Ok(_) => {
            return Ok(json!({
                "refreshed": false,
                "message": "Codex 模型目录为空",
                "count": previous["models"].as_array().map(Vec::len).unwrap_or(0),
                "models": previous["models"]
            }));
        }
        Err(error) => {
            return Ok(json!({
                "refreshed": false,
                "message": error.message,
                "count": previous["models"].as_array().map(Vec::len).unwrap_or(0),
                "models": previous["models"]
            }));
        }
    };

    let current = sdk::account()?;
    if current["secret"]["accessToken"] != credentials["accessToken"]
        || current["secret"]["identity"] != credentials["identity"]
    {
        return Err(Error::http(409, "刷新 Codex 模型目录时账号凭据已更新，请重试"));
    }

    let models = with_image_models(models);
    let catalog = catalog_with_profiles(Value::Array(models.clone()), profiles, true, sdk::now());
    let before = load()?;
    let _ = state::compare_exchange(
        Scope::Account,
        KEY,
        1,
        before.revision,
        json!({ "source": identity(&credentials), "catalog": catalog }),
        TTL,
    )?;
    Ok(sdk::catalog::refreshed(models))
}

fn text<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .filter_map(|field| value.get(*field)?.as_str())
        .map(str::trim)
        .find(|value| !value.is_empty())
}

fn context_limit(value: &Value) -> i64 {
    ["context_window", "max_context_window", "maxInputTokens", "contextWindow"]
        .into_iter()
        .find_map(|key| {
            value
                .get(key)?
                .as_i64()
                .or_else(|| value.get(key)?.as_f64().map(|n| n as i64))
                .filter(|n| *n > 0)
        })
        .unwrap_or(272_000)
}

fn items(report: &Value) -> Option<&Vec<Value>> {
    report["models"]
        .as_array()
        .or_else(|| report.pointer("/data/models")?.as_array())
        .or_else(|| report.pointer("/catalog/models")?.as_array())
}

fn parse(report: &Value) -> Result<(Vec<Value>, Value)> {
    let Some(items) = items(report) else {
        return Err(Error::http(502, "Codex 模型目录格式无效"));
    };
    let mut models = Vec::new();
    let mut profiles = serde_json::Map::new();
    for item in items {
        let visibility = item["visibility"].as_str().unwrap_or("list");
        if matches!(visibility, "hide" | "hidden") {
            continue;
        }
        let Some(id) = text(item, &["slug", "id", "name"]) else {
            continue;
        };
        let name = text(item, &["display_name", "name", "slug", "id"]).unwrap_or(id);
        // Prefer the larger window when both are present (matches Codex catalog).
        let limit = ["max_context_window", "context_window", "maxInputTokens", "contextWindow"]
            .into_iter()
            .find_map(|key| {
                item.get(key)?
                    .as_i64()
                    .or_else(|| item.get(key)?.as_f64().map(|n| n as i64))
                    .filter(|n| *n > 0)
            })
            .unwrap_or_else(|| context_limit(item));
        let mut row = model(id, name, limit);
        row["protocol"] = json!("responses");
        row["supportsToolCall"] = json!(true);
        if let Some(description) = text(item, &["description"]) {
            row["description"] = json!(description);
        }
        if let Some(modalities) = item.get("input_modalities").and_then(Value::as_array) {
            let images = modalities.iter().any(|value| value.as_str() == Some("image"));
            row["supportsImages"] = json!(images);
            row["capabilities"]["supportsImages"] = json!(images);
        }
        if let Some(levels) = item
            .get("supported_reasoning_levels")
            .or_else(|| item.get("supported_reasoning_efforts"))
        {
            row["reasoning"] = levels.clone();
            let names = reasoning_level_names(levels);
            if !names.is_empty() {
                row["supportsReasoning"] = json!(true);
                row["reasoningLevels"] =
                    Value::Array(names.into_iter().map(Value::String).collect());
            }
        }
        if let Some(default) = text(
            item,
            &[
                "default_reasoning_level",
                "default_reasoning_effort",
                "reasoning_effort",
            ],
        ) {
            row["defaultReasoning"] = json!(default);
            row["reasoningDefaultLevel"] = json!(default);
        }
        if let Some(tiers) = item.get("service_tiers").and_then(Value::as_array) {
            let ids: Vec<Value> = tiers
                .iter()
                .filter_map(|tier| tier.get("id").cloned())
                .collect();
            if !ids.is_empty() {
                row["serviceTiers"] = Value::Array(ids);
            }
        }
        let mut profile = json!({});
        if let Some(instructions) = text(item, &["base_instructions", "baseInstructions"]) {
            profile["baseInstructions"] = json!(instructions);
        }
        if item
            .get("support_verbosity")
            .or_else(|| item.get("supports_verbosity"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            profile["supportVerbosity"] = json!(true);
            if let Some(verbosity) = text(item, &["default_verbosity", "defaultVerbosity"]) {
                profile["defaultVerbosity"] = json!(verbosity);
            }
        }
        if profile.as_object().is_some_and(|object| !object.is_empty()) {
            profiles.insert(id.to_string(), profile);
        }
        models.push(row);
    }
    Ok((models, Value::Object(profiles)))
}

fn reasoning_level_names(levels: &Value) -> Vec<String> {
    let Some(items) = levels.as_array() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for item in items {
        let name = item
            .as_str()
            .or_else(|| item["effort"].as_str())
            .or_else(|| item["level"].as_str())
            .or_else(|| item["name"].as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(name) = name {
            let normalized = if name.eq_ignore_ascii_case("ultra") {
                "max".to_string()
            } else {
                name.to_ascii_lowercase()
            };
            if !names.iter().any(|existing| existing == &normalized) {
                names.push(normalized);
            }
        }
    }
    names
}

fn catalog_levels_for(model: &str) -> Option<Vec<String>> {
    if model.is_empty() {
        return None;
    }
    let catalog = cached_for(&sdk::account().ok()?["secret"]).or_else(|| {
        sdk::catalog::cached()
            .ok()
            .filter(|value| value["models"].is_array())
    })?;
    let row = catalog["models"].as_array()?.iter().find(|row| {
        row["id"]
            .as_str()
            .is_some_and(|id| id.eq_ignore_ascii_case(model))
    })?;
    row.get("reasoningLevels")
        .and_then(Value::as_array)
        .map(|levels| {
            levels
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|levels| !levels.is_empty())
        .or_else(|| {
            row.get("reasoning")
                .map(reasoning_level_names)
                .filter(|levels| !levels.is_empty())
        })
}

/// Host quota cooldown list. Image and chat buckets stay isolated.
pub fn cooldown_models(params: &Value) -> Value {
    let model = params["model"]
        .as_str()
        .or_else(|| params["wire_model"].as_str())
        .unwrap_or("")
        .trim();
    if images::is_model(model) {
        return json!({ "models": [model] });
    }
    let mut cooled = Vec::new();
    let catalog = sdk::account()
        .ok()
        .and_then(|account| cached_for(&account["secret"]))
        .or_else(|| {
            sdk::catalog::cached()
                .ok()
                .filter(|value| value["models"].is_array())
        });
    if let Some(catalog) = catalog {
        for row in catalog["models"].as_array().into_iter().flatten() {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            if images::is_model(id) || row["kind"] == "image" {
                continue;
            }
            cooled.push(Value::String(id.to_string()));
        }
    }
    if !model.is_empty()
        && !cooled
            .iter()
            .any(|value| value.as_str() == Some(model))
    {
        cooled.push(Value::String(model.to_string()));
    }
    json!({ "models": cooled })
}

/// Host model-rule binding → inject `reasoning_effort` / `reasoning.effort`.
pub fn reasoning_patch(params: &Value) -> Value {
    let body = &params["body"];
    let client_set = body
        .get("reasoning_effort")
        .is_some_and(|value| !value.is_null())
        || body
            .get("reasoning")
            .and_then(|value| value.get("effort"))
            .is_some_and(|value| !value.is_null());
    if client_set {
        return json!({
            "action": "skip",
            "reason": "客户端已指定思考档位，绑定不覆盖"
        });
    }

    let raw = params["level"].as_str().unwrap_or("").trim();
    if raw.is_empty()
        || raw.eq_ignore_ascii_case("off")
        || raw.eq_ignore_ascii_case("none")
        || raw.eq_ignore_ascii_case("auto")
    {
        return json!({
            "action": "skip",
            "reason": "关闭或自动思考的绑定保持客户端/插件默认设置"
        });
    }
    let level = if raw.eq_ignore_ascii_case("ultra") {
        "max".to_string()
    } else {
        raw.to_ascii_lowercase()
    };

    let model = params["model"]
        .as_str()
        .or_else(|| body["model"].as_str())
        .unwrap_or("");
    match catalog_levels_for(model) {
        Some(levels) if !levels.iter().any(|known| known.eq_ignore_ascii_case(&level)) => {
            return json!({
                "action": "skip",
                "reason": "Codex 账号目录尚未声明这个思考等级"
            });
        }
        None
            if !["minimal", "low", "medium", "high", "xhigh", "max"]
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&level)) =>
        {
            return json!({
                "action": "skip",
                "reason": "此档位不在通用候选内，模型目录未确认支持"
            });
        }
        _ => {}
    }

    // Responses-shaped bodies already use `input` / `reasoning.effort`.
    if body.get("input").is_some() && body.get("messages").is_none() {
        let mut reasoning = body
            .get("reasoning")
            .filter(|value| value.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        reasoning["effort"] = json!(level);
        return json!({
            "action": "set",
            "field": "reasoning",
            "value": reasoning
        });
    }

    json!({
        "action": "set",
        "field": "reasoning_effort",
        "value": level
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_catalog() {
        let (models, profiles) = parse(&json!({
            "models": [
                {
                    "slug": "gpt-6.2-sol",
                    "display_name": "GPT-6.2-Sol",
                    "context_window": 500000,
                    "max_context_window": 872000,
                    "default_reasoning_level": "low",
                    "supported_reasoning_levels": [{"effort":"low"}],
                    "input_modalities": ["text", "image"],
                    "service_tiers": [{"id":"priority"}],
                    "base_instructions": "stay concise",
                    "support_verbosity": true,
                    "default_verbosity": "medium"
                },
                { "slug": "hidden", "visibility": "hide" },
                { "id": "gpt-6-luna", "name": "Luna" }
            ]
        }))
        .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "gpt-6.2-sol");
        assert_eq!(models[0]["capabilities"]["maxInputTokens"], 872000);
        assert_eq!(models[0]["defaultReasoning"], "low");
        assert_eq!(models[0]["reasoningLevels"], json!(["low"]));
        assert_eq!(models[0]["supportsImages"], true);
        assert_eq!(models[0]["serviceTiers"], json!(["priority"]));
        assert_eq!(profiles["gpt-6.2-sol"]["baseInstructions"], "stay concise");
        assert_eq!(profiles["gpt-6.2-sol"]["defaultVerbosity"], "medium");
        assert_eq!(models[1]["id"], "gpt-6-luna");
        assert_eq!(models[1]["name"], "Luna");
    }

    #[test]
    fn cooldown_isolates_image_from_chat() {
        let image = cooldown_models(&json!({ "model": "gpt-image-1.5" }));
        assert_eq!(image["models"], json!(["gpt-image-1.5"]));

        let chat = cooldown_models(&json!({ "wire_model": "gpt-5.1-codex" }));
        assert_eq!(chat["models"], json!(["gpt-5.1-codex"]));
        assert!(!chat["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("gpt-image-1.5")));
    }

    #[test]
    fn reasoning_patch_sets_or_skips() {
        let skip = reasoning_patch(&json!({
            "level": "high",
            "body": { "reasoning_effort": "low", "model": "gpt-6.2-sol" }
        }));
        assert_eq!(skip["action"], "skip");

        let set = reasoning_patch(&json!({
            "level": "ultra",
            "body": { "model": "unknown-model", "messages": [] }
        }));
        assert_eq!(set["action"], "set");
        assert_eq!(set["field"], "reasoning_effort");
        assert_eq!(set["value"], "max");

        let responses = reasoning_patch(&json!({
            "level": "medium",
            "body": { "model": "x", "input": [] }
        }));
        assert_eq!(responses["action"], "set");
        assert_eq!(responses["field"], "reasoning");
        assert_eq!(responses["value"]["effort"], "medium");
    }
}
