//! Codex subscription allowance and purchased credit reporting.
//! Report normalization adapted from Agent2API v3.5.1, AGPL-3.0-only.

use super::{auth, config, PROVIDER};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

fn headers(access: &str, account: &str) -> Value {
    let originator = config::originator(false).unwrap_or_else(|_| config::DEFAULT_ORIGINATOR.into());
    json!({
        "Authorization": format!("Bearer {access}"),
        "ChatGPT-Account-Id": account,
        "originator": originator,
        "User-Agent": concat!("Halo-Codex/", env!("CARGO_PKG_VERSION")),
        "Accept": "application/json",
    })
}

/// ChatGPT Plus/Pro subscription end time from `/subscriptions` (not present on `/wham/usage`).
fn subscription_info(access: &str, account: &str) -> Option<Value> {
    let url = format!("https://chatgpt.com/backend-api/subscriptions?account_id={account}");
    let report = http::open("GET", &url, headers(access, account), None)
        .ok()?
        .json()
        .ok()?;
    let plan = text(
        &report,
        &[
            "plan_type",
            "subscription_plan",
            "account_plan_type",
            "planType",
        ],
    );
    let expire_at = ["active_until", "expires_at", "renews_at", "next_renewal_at"]
        .into_iter()
        .find_map(|key| {
            let value = report.get(key)?;
            if let Some(text) = value.as_str() {
                return chrono::DateTime::parse_from_rfc3339(text)
                    .ok()
                    .map(|date| date.timestamp_millis());
            }
            let at = value.as_i64().or_else(|| value.as_str()?.parse().ok())?;
            if at <= 0 {
                return None;
            }
            if at < 100_000_000_000 {
                at.checked_mul(1000)
            } else {
                Some(at)
            }
        });
    let will_renew = report
        .get("will_renew")
        .or_else(|| report.get("willRenew"))
        .and_then(Value::as_bool);
    if plan.is_empty() && expire_at.is_none() && will_renew.is_none() {
        return None;
    }
    let mut info = json!({});
    if !plan.is_empty() {
        info["planName"] = json!(plan);
    }
    if let Some(at) = expire_at {
        info["expireAt"] = json!(at);
    }
    if let Some(flag) = will_renew {
        info["willRenew"] = json!(flag);
    }
    Some(info)
}

fn merge_subscription(report: &mut Value, extra: Option<Value>) {
    let Some(extra) = extra else {
        return;
    };
    let mut subscription = match report.get("subscription") {
        Some(value) if value.is_object() => value.clone(),
        _ => json!({}),
    };
    for key in ["planName", "expireAt", "willRenew"] {
        if let Some(value) = extra.get(key) {
            if key == "planName"
                && subscription
                    .get("planName")
                    .and_then(Value::as_str)
                    .is_some_and(|plan| !plan.is_empty())
            {
                continue;
            }
            subscription[key] = value.clone();
        }
    }
    if subscription.as_object().is_some_and(|object| !object.is_empty()) {
        report["subscription"] = subscription;
    }
}

pub fn query() -> Result<Value> {
    let credentials = auth::credentials(false)?;
    let access = credentials["accessToken"]
        .as_str()
        .ok_or_else(|| Error::http(401, "请重新授权 Codex"))?;
    let account = credentials["accountId"]
        .as_str()
        .ok_or_else(|| Error::http(401, "Codex 凭据缺少账号标识"))?;
    let report = http::open(
        "GET",
        "https://chatgpt.com/backend-api/wham/usage",
        headers(access, account),
        None,
    )?
    .json()?;
    let subscription = subscription_info(access, account);
    let current = sdk::account()?;
    if current["secret"]["accessToken"] != credentials["accessToken"]
        || current["secret"]["identity"] != credentials["identity"]
    {
        return Err(Error::http(409, "查询 Codex 额度时账号凭据已更新，请重试"));
    }
    let mut parsed = parse(&report)?;
    merge_subscription(&mut parsed, subscription);
    Ok(parsed)
}

/// Host `admin.action` → `quota.sources` wallet breakdown from a usage report.
pub fn sources(report: &Value) -> Value {
    report
        .get("quotaSources")
        .filter(|value| value.is_array())
        .cloned()
        .unwrap_or_else(|| json!([]))
}

fn text<'a>(value: &'a Value, fields: &[&str]) -> &'a str {
    fields
        .iter()
        .filter_map(|field| value.get(*field)?.as_str())
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or_default()
}

fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|number| number.is_finite())
}

fn reset(window: &Value) -> Option<i64> {
    ["reset_at", "resets_at", "resetAt"]
        .into_iter()
        .find_map(|key| {
            let value = window.get(key)?;
            if let Some(text) = value.as_str() {
                if let Ok(date) = chrono::DateTime::parse_from_rfc3339(text) {
                    return Some(date.timestamp_millis());
                }
            }
            let at = value.as_i64().or_else(|| value.as_str()?.parse().ok())?;
            if at <= 0 {
                return None;
            }
            if at < 100_000_000_000 {
                at.checked_mul(1000)
            } else {
                Some(at)
            }
        })
        .or_else(|| {
            [
                "reset_after_seconds",
                "resets_in_seconds",
                "retry_after_seconds",
            ]
            .into_iter()
            .find_map(|key| {
                let seconds = number(window.get(key))?;
                if !(0.0..=31_536_000.0).contains(&seconds) {
                    return None;
                }
                sdk::now().checked_add((seconds * 1000.0) as i64)
            })
        })
}

fn window_name(seconds: Option<f64>, fallback: &str) -> (String, String) {
    match seconds {
        Some(seconds) if seconds == 18_000.0 => ("5 小时".into(), "5h".into()),
        Some(seconds) if seconds == 604_800.0 => ("每周".into(), "weekly".into()),
        Some(seconds) if seconds >= 86_400.0 && seconds % 86_400.0 == 0.0 => {
            (format!("{} 天", seconds / 86_400.0), fallback.into())
        }
        Some(seconds) if seconds >= 3600.0 && seconds % 3600.0 == 0.0 => {
            (format!("{} 小时", seconds / 3600.0), fallback.into())
        }
        _ => (
            if fallback == "primary" {
                "主要额度窗口"
            } else {
                "次要额度窗口"
            }
            .into(),
            fallback.into(),
        ),
    }
}

fn add_window(
    key: &str,
    prefix: &str,
    raw: &Value,
    model: Option<&str>,
    windows: &mut Vec<Value>,
    wallets: &mut Vec<Value>,
    sources: &mut Vec<Value>,
) {
    if !raw.is_object() {
        return;
    }
    let seconds = number(raw.get("limit_window_seconds"))
        .or_else(|| number(raw.get("window_minutes")).map(|minutes| minutes * 60.0));
    let (label, window) = window_name(seconds, key);
    let label = if prefix.is_empty() {
        label
    } else {
        format!("{prefix} · {label}")
    };
    let id = if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}:{key}")
    };
    let used = number(raw.get("used_percent")).filter(|used| *used >= 0.0);
    let remaining = used.map(|used| (100.0 - used).clamp(0.0, 100.0));
    let resets_at = reset(raw);
    let mut row = json!({ "key": id, "label": label, "window": window });
    if let Some(seconds) = seconds {
        row["windowSeconds"] = json!(seconds);
    }
    if let Some(used) = used {
        row["usedPercent"] = json!(used);
    }
    if let Some(remaining) = remaining {
        row["remainingPercent"] = json!(remaining);
        row["view"] = json!(format!("剩 {:.1}%", remaining));
        wallets.push(json!({ "type": id, "displayName": label, "balance": remaining, "unit": "%", "resetsAt": resets_at }));
    }
    if let Some(resets_at) = resets_at {
        row["resetsAt"] = json!(resets_at);
    }
    windows.push(row);
    let mut source = json!({ "id": format!("window:{id}"), "provider": PROVIDER, "label": label,
        "unit": "%", "remainingPercent": remaining });
    if let Some(resets_at) = resets_at {
        source["resetsAt"] = json!(resets_at);
    }
    if let Some(model) = model {
        source["models"] = json!([model]);
    }
    sources.push(source);
}

fn parse(raw: &Value) -> Result<Value> {
    // Halo Codex only forwards ChatGPT/Codex subscription rate windows.
    // Purchased credits / 额外点数 are intentionally ignored.
    if !raw.is_object()
        || ["rate_limit", "plan_type"]
            .iter()
            .all(|key| raw.get(*key).is_none())
    {
        return Err(Error::http(
            502,
            "Codex 没有返回可识别的订阅额度数据，未把未知额度记为零",
        ));
    }
    let plan = text(raw, &["plan_type"]);
    let mut windows = Vec::new();
    let mut wallets = Vec::new();
    let mut sources = Vec::new();
    if let Some(rate) = raw.get("rate_limit") {
        for (key, field) in [
            ("primary", "primary_window"),
            ("secondary", "secondary_window"),
        ] {
            if let Some(window) = rate.get(field) {
                add_window(
                    key,
                    "",
                    window,
                    None,
                    &mut windows,
                    &mut wallets,
                    &mut sources,
                );
            }
        }
    }
    let additional = raw.get("additional_rate_limits").and_then(Value::as_array);
    for rate in additional.into_iter().flatten() {
        let label = text(rate, &["limit_name", "metered_feature", "name"]);
        if label.is_empty() {
            continue;
        }
        let data = rate.get("rate_limit").unwrap_or(rate);
        let model = data.get("model").and_then(Value::as_str);
        for (key, field) in [
            ("primary", "primary_window"),
            ("secondary", "secondary_window"),
        ] {
            if let Some(window) = data.get(field) {
                add_window(
                    key,
                    label,
                    window,
                    model,
                    &mut windows,
                    &mut wallets,
                    &mut sources,
                );
            }
        }
    }
    let available = windows
        .iter()
        .filter_map(|window| window.get("remainingPercent").and_then(Value::as_f64))
        .reduce(f64::min);
    let mut report = json!({
        "available": available,
        "unit": "%",
        "windows": windows,
        "wallets": wallets,
        "quotaSources": sources,
        "credits": [],
        "subscription": if plan.is_empty() { Value::Null } else { json!({ "planName": plan }) },
        "allowed": raw.pointer("/rate_limit/allowed").and_then(Value::as_bool),
    });
    // Keep only subscription rate-limit trees for diagnostics — no credits payload.
    report["raw"] = json!({
        "plan_type": plan,
        "rate_limit": raw.get("rate_limit"),
        "additional_rate_limits": raw.get("additional_rate_limits"),
        "rate_limit_reached_type": raw.get("rate_limit_reached_type"),
    });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_allowance_windows_and_reset_times() {
        let report = parse(&json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "primary_window": {"used_percent": "25", "limit_window_seconds": 18000, "reset_at": 1800000000},
                "secondary_window": {"used_percent": 60, "limit_window_seconds": 604800, "reset_at": 1800100000000_i64}
            }
        })).unwrap();
        assert_eq!(report["available"], json!(40.0));
        assert_eq!(report["windows"][0]["window"], "5h");
        assert_eq!(report["windows"][1]["window"], "weekly");
        assert_eq!(report["wallets"][0]["resetsAt"], 1800000000000_i64);
        assert_eq!(report["quotaSources"][0]["provider"], PROVIDER);
    }

    #[test]
    fn ignores_purchased_credits_and_reports_subscription_windows_only() {
        let report = parse(&json!({
            "rate_limit": {"primary_window": {"used_percent": 100, "limit_window_seconds": 18000}},
            "credits": {"has_credits": true, "unlimited": false, "balance": "12.5"}
        }))
        .unwrap();
        assert_eq!(report["available"], json!(0.0));
        assert_eq!(report["windows"][0]["remainingPercent"], json!(0.0));
        assert!(report["credits"].as_array().unwrap().is_empty());
        assert_eq!(report["wallets"].as_array().unwrap().len(), 1);
        assert_eq!(report["wallets"][0]["unit"], "%");
        assert!(report.get("creditsBalance").is_none());
        assert!(report.get("hasCredits").is_none());
    }

    #[test]
    fn leaves_missing_allowance_unknown() {
        assert!(parse(&json!({})).is_err());
        assert!(parse(&json!({"credits": {"balance": 1}})).is_err());
        let report = parse(&json!({"plan_type": "plus"})).unwrap();
        assert!(report["available"].is_null());
        assert!(report["windows"].as_array().unwrap().is_empty());
    }

    #[test]
    fn merges_subscription_expiry() {
        let mut report = parse(&json!({
            "plan_type": "plus",
            "rate_limit": {"primary_window": {"used_percent": 10, "limit_window_seconds": 18000}}
        }))
        .unwrap();
        merge_subscription(
            &mut report,
            Some(json!({
                "planName": "plus",
                "expireAt": 1800000000000_i64,
                "willRenew": true
            })),
        );
        assert_eq!(report["subscription"]["planName"], "plus");
        assert_eq!(report["subscription"]["expireAt"], 1800000000000_i64);
        assert_eq!(report["subscription"]["willRenew"], true);
    }
}
