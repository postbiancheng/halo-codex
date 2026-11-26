//! Codex / ChatGPT upstream error classification for the Agent2API host.

use agent2api_wasm_sdk::{self as sdk, json, Value};

fn message(body: &Value) -> String {
    [
        "/error/message",
        "/message",
        "/error",
        "/detail",
        "/response/error/message",
    ]
    .iter()
    .find_map(|path| {
        body.pointer(path)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    })
    .or_else(|| body.as_str().map(str::trim).filter(|text| !text.is_empty()))
    .unwrap_or("Codex 上游未返回错误说明")
    .to_string()
}

fn code_text(body: &Value) -> String {
    [
        body.get("code"),
        body.pointer("/error/code"),
        body.pointer("/response/error/code"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| value.as_str().map(str::trim).filter(|text| !text.is_empty()))
    .unwrap_or("")
    .to_ascii_lowercase()
}

fn reset_at(body: &Value) -> Option<i64> {
    let now = sdk::now();
    [
        body.get("reset_at"),
        body.get("resetAt"),
        body.get("resets_at"),
        body.pointer("/error/reset_at"),
        body.pointer("/error/resets_at"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        let at = value
            .as_i64()
            .or_else(|| value.as_f64().map(|n| n as i64))
            .or_else(|| value.as_str()?.parse().ok())?;
        if at <= 0 {
            return None;
        }
        let at = if at < 100_000_000_000 {
            at.checked_mul(1000)?
        } else {
            at
        };
        (at > now).then_some(at)
    })
    .or_else(|| {
        let seconds = [
            body.get("retry_after"),
            body.get("retryAfter"),
            body.pointer("/error/retry_after"),
        ]
        .into_iter()
        .flatten()
        .find_map(|value| {
            value
                .as_u64()
                .or_else(|| value.as_f64().map(|n| n as u64))
                .or_else(|| value.as_str()?.parse().ok())
        })?;
        if seconds == 0 || seconds > 7 * 86400 {
            return None;
        }
        now.checked_add((seconds as i64).saturating_mul(1000))
    })
}

pub fn classify(status: u16, body: &Value) -> Value {
    let raw = message(body);
    let lower = raw.to_ascii_lowercase();
    let code = code_text(body);
    let failed = if (400..600).contains(&status) {
        status
    } else {
        502
    };

    if status == 401
        || matches!(
            code.as_str(),
            "unauthorized"
                | "invalid_api_key"
                | "authentication_error"
                | "token_expired"
                | "codex_login_required"
        )
        || lower.contains("login") && lower.contains("expir")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        || lower.contains("access token")
    {
        return json!({
            "kind": "token_expired",
            "status": 401,
            "message": format!("Codex 登录已失效：{raw}"),
            "upstream_code": code,
        });
    }

    let quota = matches!(
        code.as_str(),
        "insufficient_quota"
            | "quota_exceeded"
            | "billing_not_active"
            | "usage_limit_exceeded"
            | "rate_limit_exceeded"
    ) || [
        "insufficient quota",
        "quota exceeded",
        "usage limit",
        "rate limit",
        "too many requests",
        "额度",
        "用量已用完",
        "速率限制",
    ]
    .iter()
    .any(|part| lower.contains(part))
        || status == 429;

    if quota {
        let transient = status == 429
            && !["insufficient_quota", "quota_exceeded", "billing_not_active", "usage_limit_exceeded"]
                .contains(&code.as_str())
            && !lower.contains("insufficient quota")
            && !lower.contains("usage limit");
        return json!({
            "kind": "quota_limited",
            "status": 429,
            "message": format!("Codex 上游限制：{raw}"),
            "upstream_code": code,
            "reset_at": reset_at(body).or_else(|| transient.then(|| sdk::now().saturating_add(60_000))),
        });
    }

    let blocked = matches!(
        code.as_str(),
        "content_filter" | "content_policy_violation" | "responsible_ai_policy"
    ) || [
        "content filter",
        "content policy",
        "safety system",
        "blocked by",
    ]
    .iter()
    .any(|part| lower.contains(part));

    if blocked {
        return json!({
            "kind": "content_blocked",
            "status": failed,
            "message": format!("Codex 内容策略拒绝：{raw}；请调整内容后重试"),
            "upstream_code": code,
        });
    }

    json!({
        "kind": "fatal",
        "status": failed,
        "message": format!("Codex 上游请求失败：{raw}"),
        "upstream_code": code,
        "reset_at": reset_at(body),
    })
}

pub fn policy(status: u16, body: &Value) -> Value {
    let class = classify(status, body);
    let kind = class["kind"].as_str().unwrap_or("fatal");
    json!({
        "stop": matches!(kind, "token_expired" | "content_blocked"),
        "kind": kind,
    })
}

pub fn retry(params: &Value) -> Value {
    let attempt = params["attempt"].as_u64().unwrap_or(0);
    let budget = params["budget"].as_u64().unwrap_or(0);
    if budget > 0 && attempt >= budget {
        return json!({ "action": "stop", "reason": "已达到重试上限" });
    }
    let status = params["status"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(502);
    let class = classify(status, &params["body"]);
    match class["kind"].as_str().unwrap_or("fatal") {
        "token_expired" | "content_blocked" => json!({
            "action": "stop",
            "reason": class["message"].as_str().unwrap_or("不可重试的上游错误"),
        }),
        "quota_limited" => {
            let delay = class["reset_at"]
                .as_i64()
                .map(|at| at.saturating_sub(sdk::now()).max(0) as u64)
                .filter(|ms| *ms > 0 && *ms <= 15 * 60_000)
                .unwrap_or(10_000);
            json!({
                "action": "retry",
                "delay_ms": delay,
                "reason": "Codex 限流或额度窗口，等待后重试",
            })
        }
        _ if matches!(status, 408 | 425 | 500 | 502 | 503 | 504) => json!({
            "action": "retry",
            "delay_ms": 2_000u64.saturating_mul(attempt.saturating_add(1).min(5)),
            "reason": "Codex 上游暂时失败，短暂重试",
        }),
        _ => json!({ "action": "default" }),
    }
}

pub fn advice(params: &Value) -> Value {
    let decision = retry(params);
    if decision["action"] == "retry" {
        json!({
            "delay_ms": decision["delay_ms"],
            "reason": decision["reason"],
        })
    } else {
        Value::Null
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auth_and_quota() {
        let auth = classify(401, &json!({"error":{"message":"Unauthorized","code":"invalid_api_key"}}));
        assert_eq!(auth["kind"], "token_expired");

        let quota = classify(429, &json!({"error":{"message":"Rate limit exceeded","code":"rate_limit_exceeded"}}));
        assert_eq!(quota["kind"], "quota_limited");
        assert!(quota.get("reset_at").is_some());

        let blocked = classify(
            400,
            &json!({"error":{"message":"Blocked by safety system","code":"content_filter"}}),
        );
        assert_eq!(blocked["kind"], "content_blocked");
    }
}
