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
    json!({ "models": models, "meta": { "remote": remote, "refreshed_at": refreshed_at } })
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
    let user_agent = format!("{originator}/{client_version}");
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
            "User-Agent": user_agent,
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

    let models = match parse(&report) {
        Ok(models) if !models.is_empty() => models,
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
    let catalog = catalog(Value::Array(models.clone()), true, sdk::now());
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

fn parse(report: &Value) -> Result<Vec<Value>> {
    let Some(items) = items(report) else {
        return Err(Error::http(502, "Codex 模型目录格式无效"));
    };
    let mut models = Vec::new();
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
        if let Some(description) = text(item, &["description"]) {
            row["description"] = json!(description);
        }
        if let Some(levels) = item
            .get("supported_reasoning_levels")
            .or_else(|| item.get("supported_reasoning_efforts"))
        {
            row["reasoning"] = levels.clone();
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
        }
        models.push(row);
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_catalog() {
        let models = parse(&json!({
            "models": [
                {
                    "slug": "gpt-6.2-sol",
                    "display_name": "GPT-6.2-Sol",
                    "context_window": 500000,
                    "max_context_window": 872000,
                    "default_reasoning_level": "low",
                    "supported_reasoning_levels": [{"effort":"low"}]
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
        assert_eq!(models[1]["id"], "gpt-6-luna");
        assert_eq!(models[1]["name"], "Luna");
    }
}
