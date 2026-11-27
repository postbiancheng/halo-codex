//! Codex native image generation / edits via `/codex/images/*`.

use super::{auth, config, errors, PROVIDER};
use agent2api_wasm_sdk::{self as sdk, http, images::Resource, json, Error, Result, Value};
use base64::Engine;

pub const MODELS: &[(&str, &str)] = &[
    ("gpt-image-2", "GPT Image 2"),
    ("gpt-image-1.5", "GPT Image 1.5"),
];

const MAX_RESPONSE: usize = 12 * 1024 * 1024;

pub fn catalog_rows() -> Vec<Value> {
    MODELS
        .iter()
        .map(|(id, name)| {
            json!({
                "id": id,
                "name": name,
                "kind": "image",
                "provider": PROVIDER,
                "synthetic": true,
                "output_modalities": ["image"],
                "input_modalities": ["text", "image"],
                "supportsImages": true,
                "supports_image_generation": true,
                "supports_image_editing": true,
                "supportsToolCall": false,
                "supportsReasoning": false
            })
        })
        .collect()
}

pub fn is_model(model: &str) -> bool {
    MODELS.iter().any(|(id, _)| id.eq_ignore_ascii_case(model))
}

fn supported(model: &str) -> bool {
    is_model(model)
}

fn uncertain(message: &str) -> Error {
    Error::http(
        502,
        format!("Codex 图片结果尚未确认：{message}；未自动重新提交，以免重复消耗额度"),
    )
    .with_code("image_output_uncertain")
}

fn mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
}

fn headers(secret: &Value) -> Result<Value> {
    let token = secret["accessToken"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::http(401, "Codex 登录已过期").with_code("codex_login_required"))?;
    let account = secret["accountId"].as_str().unwrap_or_default();
    if account.is_empty() || secret["expiresAt"].as_i64().unwrap_or(0) <= sdk::now() {
        return Err(Error::http(401, "Codex 登录已过期").with_code("codex_login_required"));
    }
    let originator = config::originator(false)?;
    let version = config::client_version()?;
    let request_id = sdk::host("random.bytes", json!({"length":12}))?
        .as_str()
        .unwrap_or("img")
        .replace('+', "-")
        .replace('/', "_");
    Ok(json!({
        "Authorization": format!("Bearer {token}"),
        "ChatGPT-Account-Id": account,
        "chatgpt-account-id": account,
        "originator": originator,
        "User-Agent": config::user_agent(false)?,
        "version": version,
        "Accept": "application/json",
        "Content-Type": "application/json",
        "x-client-request-id": format!("img_{request_id}"),
    }))
}

fn prompt(body: &Value) -> Result<String> {
    body["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::http(400, "图片请求缺少 prompt"))
}

fn size(body: &Value) -> Option<String> {
    body["size"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("auto"))
        .map(str::to_owned)
}

fn reference_urls(body: &Value) -> Result<Vec<String>> {
    let images = body["images"]
        .as_array()
        .filter(|images| !images.is_empty() && images.len() <= 16)
        .ok_or_else(|| Error::http(400, "图片编辑需要 1 至 16 张参考图"))?;
    images
        .iter()
        .map(|image| {
            let url = image
                .get("image_url")
                .and_then(|value| {
                    value
                        .as_str()
                        .or_else(|| value.get("url").and_then(Value::as_str))
                })
                .or_else(|| image.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| Error::http(400, "参考图缺少 image_url"))?;
            let valid = url.starts_with("data:image/")
                || url::Url::parse(url)
                    .is_ok_and(|parsed| parsed.scheme() == "https" && parsed.host_str().is_some());
            if !valid {
                return Err(Error::http(400, "参考图需要 data URL 或 https 地址"));
            }
            Ok(url.to_string())
        })
        .collect()
}

fn pack_result(payload: &Value) -> Result<Value> {
    let rows = payload["data"]
        .as_array()
        .filter(|rows| !rows.is_empty() && rows.len() <= 16)
        .ok_or_else(|| uncertain("上游未返回有效图片数组"))?;
    let mut data = Vec::with_capacity(rows.len());
    for row in rows {
        let mut image = if let Some(url) = row["url"].as_str().filter(|url| !url.is_empty()) {
            Resource::fetch_public(url)
                .and_then(Resource::image_result)
                .map_err(|_| uncertain("生成图片未能安全下载"))?
        } else if let Some(encoded) = row["b64_json"].as_str().filter(|value| !value.is_empty()) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| uncertain("图片内容不是有效 base64"))?;
            let resource = Resource::create(
                mime(&bytes).ok_or_else(|| uncertain("图片格式不是 PNG、JPEG 或 WebP"))?,
            )?;
            resource.append(&bytes)?;
            resource.image_result()?
        } else {
            return Err(uncertain("图片条目缺少 url 或 b64_json"));
        };
        if let Some(prompt) = row.get("revised_prompt").filter(|value| value.is_string()) {
            image["revised_prompt"] = prompt.clone();
        }
        data.push(image);
    }
    let mut result = json!({
        "created": payload["created"].as_i64().unwrap_or_else(|| sdk::now() / 1000),
        "data": data
    });
    if let Some(usage) = payload.get("usage") {
        result["usage"] = usage.clone();
    }
    for key in ["size", "quality", "background", "output_format"] {
        if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
            result[key] = value.clone();
        }
    }
    Ok(result)
}

fn response_payload(status: u16, bytes: Result<Vec<u8>>) -> Result<Value> {
    let parsed = bytes.and_then(|bytes| {
        serde_json::from_slice::<Value>(&bytes).map_err(|_| Error::new("上游没有返回有效 JSON"))
    });
    if (200..300).contains(&status) {
        let payload = parsed.map_err(|error| uncertain(&error.message))?;
        if payload.get("error").is_none_or(Value::is_null) {
            return Ok(payload);
        }
        return Err(image_error(status, &payload));
    }
    Err(image_error(status, &parsed.unwrap_or(Value::Null)))
}

fn image_error(status: u16, payload: &Value) -> Error {
    let class = errors::classify(status, payload);
    Error::http(
        class["status"].as_u64().unwrap_or(status as u64) as u16,
        class["message"].as_str().unwrap_or("Codex 图片请求失败"),
    )
    .with_code("codex_image_error")
    .with_reset(class["reset_at"].as_i64())
}

pub fn generate(params: &Value, editing: bool) -> Result<Value> {
    let body = &params["body"];
    let model = body["model"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::http(400, "图片请求缺少 model"))?;
    if !supported(model) {
        return Err(Error::http(
            400,
            format!("Codex 图片请使用 {} / {}", MODELS[0].0, MODELS[1].0),
        ));
    }
    if body
        .get("n")
        .is_some_and(|n| !n.as_u64().is_some_and(|n| (1..=4).contains(&n)))
    {
        return Err(Error::http(400, "Codex 图片数量 n 需为 1 至 4"));
    }
    if body.get("mask").is_some_and(|value| !value.is_null()) {
        return Err(Error::http(400, "Codex 图片编辑暂不支持 mask"));
    }

    let secret = auth::credentials(false)?;
    let prompt = prompt(body)?;
    let mut outbound = json!({
        "model": model,
        "prompt": prompt,
        "response_format": "b64_json",
    });
    if let Some(size) = size(body) {
        outbound["size"] = json!(size);
    }
    for key in [
        "n",
        "quality",
        "background",
        "style",
        "moderation",
        "output_format",
    ] {
        if let Some(value) = body.get(key).filter(|value| !value.is_null()) {
            outbound[key] = value.clone();
        }
    }
    let path = if editing {
        outbound["images"] = json!(reference_urls(body)?);
        "https://chatgpt.com/backend-api/codex/images/edits"
    } else {
        "https://chatgpt.com/backend-api/codex/images/generations"
    };

    let response = http::open("POST", path, headers(&secret)?, Some(&outbound))
        .map_err(|_| uncertain("请求传输失败"))?;
    let payload = response_payload(response.status, response.bytes(MAX_RESPONSE))?;
    pack_result(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_error_status_when_the_image_response_cannot_be_parsed() {
        let unauthorized = response_payload(401, Ok(b"<html>".to_vec())).unwrap_err();
        assert_eq!(unauthorized.status, Some(401));
        let rate_limit = response_payload(429, Err(Error::new("EOF"))).unwrap_err();
        assert_eq!(rate_limit.status, Some(429));
        let uncertain = response_payload(200, Ok(Vec::new())).unwrap_err();
        assert_eq!(uncertain.code.as_deref(), Some("image_output_uncertain"));
    }

    #[test]
    fn classifies_image_http_errors_and_reset_times() {
        let unauthorized = response_payload(
            401,
            Ok(br#"{"error":{"message":"Unauthorized","code":"invalid_api_key"}}"#.to_vec()),
        )
        .unwrap_err();
        assert_eq!(unauthorized.status, Some(401));
        assert_eq!(unauthorized.code.as_deref(), Some("codex_image_error"));

        let reset_at = sdk::now() / 1000 + 60;
        let body = serde_json::to_vec(&json!({
            "error": {"message": "Rate limit exceeded", "code": "rate_limit_exceeded"},
            "reset_at": reset_at
        }))
        .unwrap();
        let limited = response_payload(429, Ok(body)).unwrap_err();
        assert_eq!(limited.status, Some(429));
        assert_eq!(limited.reset_at, Some(reset_at * 1000));

        let envelope = response_payload(
            200,
            Ok(br#"{"error":{"message":"generation failed"}}"#.to_vec()),
        )
        .unwrap_err();
        assert_eq!(envelope.status, Some(502));
    }

    #[test]
    fn recognizes_only_supported_image_signatures() {
        assert_eq!(mime(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(mime(b"\xff\xd8\xffrest"), Some("image/jpeg"));
        assert_eq!(mime(b"RIFF1234WEBPrest"), Some("image/webp"));
        assert_eq!(mime(b"RIFF1234WAVErest"), None);
        assert_eq!(mime(b"GIF89a"), None);
    }

    #[test]
    fn rejects_invalid_base64_and_unsupported_image_bytes() {
        let invalid = pack_result(&json!({"data":[{"b64_json":"%%%"}]})).unwrap_err();
        assert_eq!(invalid.code.as_deref(), Some("image_output_uncertain"));

        let encoded = base64::engine::general_purpose::STANDARD.encode(b"GIF89a");
        let unsupported = pack_result(&json!({"data":[{"b64_json":encoded}]})).unwrap_err();
        assert_eq!(unsupported.code.as_deref(), Some("image_output_uncertain"));

        let missing = pack_result(&json!({"data":[{}]})).unwrap_err();
        assert_eq!(missing.code.as_deref(), Some("image_output_uncertain"));
    }

    #[test]
    fn validates_reference_image_urls_and_counts() {
        let valid = reference_urls(&json!({"images":[
            "data:image/png;base64,iVBORw0KGgo=",
            {"image_url":"https://example.com/image.webp"},
            {"image_url":{"url":"https://example.com/image.jpg"}}
        ]}))
        .unwrap();
        assert_eq!(valid.len(), 3);

        for body in [
            json!({"images":[]}),
            json!({"images":["http://example.com/image.png"]}),
            json!({"images":["https://"]}),
            json!({"images":[{}]}),
        ] {
            assert!(
                reference_urls(&body).is_err(),
                "accepted invalid input: {body}"
            );
        }
        let too_many = json!({"images":vec!["data:image/png;base64,iVBORw0KGgo=";17]});
        assert!(reference_urls(&too_many).is_err());
    }

    #[test]
    fn catalogs_image_models() {
        let rows = catalog_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["kind"], "image");
        assert_eq!(rows[0]["id"], "gpt-image-2");
    }
}
