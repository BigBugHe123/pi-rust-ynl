use std::path::Path;
use std::time::Duration;

use pi_ai::auth::AuthOverrides;
use pi_coding_agent::agent_session::{ModelRuntime, ModelRuntimeOptions};
use serde_json::{Value, json};

const PROVIDERS: &[&str] = &[
    "openai-codex",
    "deepseek",
    "openrouter",
    "moonshotai",
    "moonshotai-cn",
    "minimax",
    "minimax-cn",
    "vercel-ai-gateway",
    "opencode-go",
    "kimi-coding",
];

pub fn is_provider_id(provider_id: &str) -> bool {
    PROVIDERS.contains(&provider_id)
}

pub async fn query(agent_dir: &Path, provider_id: &str) -> Value {
    if !PROVIDERS.contains(&provider_id) {
        return json!({"providerId":provider_id,"status":"query-failed","message":"The provider usage query failed."});
    }
    let runtime = ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(agent_dir)).await;
    let models = runtime.all_models();
    let model_base = models.iter().find(|model| model.provider == provider_id).map(|model| model.base_url.as_str());
    if !model_base.is_some_and(|base| official_origin(provider_id, base)) {
        return query_failed(provider_id);
    }
    let resolved = match runtime.resolver().resolve(provider_id, &AuthOverrides::default()).await {
        Ok(Some(resolved)) => resolved,
        _ => {
            return json!({"providerId":provider_id,"status":"auth-unavailable","message":"Provider authentication is unavailable."});
        }
    };
    if resolved.auth.base_url.as_deref().is_some_and(|base| !official_origin(provider_id, base)) {
        return query_failed(provider_id);
    }
    let mut endpoint = endpoint(provider_id).unwrap().to_string();
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &resolved.auth.headers {
        if let (Ok(name), Ok(value)) =
            (reqwest::header::HeaderName::try_from(name), reqwest::header::HeaderValue::try_from(value))
        {
            headers.insert(name, value);
        }
    }
    if !headers.contains_key(reqwest::header::AUTHORIZATION) {
        if let Some(key) = &resolved.auth.api_key {
            if let Ok(value) = reqwest::header::HeaderValue::try_from(format!("Bearer {key}")) {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
        }
    }
    if provider_id == "minimax" || provider_id == "minimax-cn" {
        let authorization =
            headers.get(reqwest::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let key = authorization.strip_prefix("Bearer ").or(resolved.auth.api_key.as_deref()).unwrap_or_default();
        endpoint.push_str(if key.starts_with("sk-api-") { "/account/query_balance" } else { "/v1/token_plan/remains" });
    }
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(_) => return query_failed(provider_id),
    };
    let response = match client.get(endpoint).headers(headers).send().await {
        Ok(response) if response.status().is_success() => response,
        _ => return query_failed(provider_id),
    };
    let bytes = match response.bytes().await {
        Ok(bytes) if bytes.len() <= 65_536 => bytes,
        _ => return query_failed(provider_id),
    };
    let payload = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if value.is_object() => value,
        _ => return query_failed(provider_id),
    };
    match normalize(provider_id, &payload) {
        Some(report) => json!({"providerId":provider_id,"status":"ready","report":report}),
        None => query_failed(provider_id),
    }
}

fn endpoint(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "openai-codex" => "https://chatgpt.com/backend-api/wham/usage",
        "deepseek" => "https://api.deepseek.com/user/balance",
        "openrouter" => "https://openrouter.ai/api/v1/key",
        "moonshotai" => "https://api.moonshot.ai/v1/users/me/balance",
        "moonshotai-cn" => "https://api.moonshot.cn/v1/users/me/balance",
        "minimax" => "https://api.minimax.io",
        "minimax-cn" => "https://api.minimaxi.com",
        "vercel-ai-gateway" => "https://ai-gateway.vercel.sh/v1/credits",
        "opencode-go" => "https://opencode.ai/zen/go/v1/usage",
        "kimi-coding" => "https://api.kimi.com/coding/v1/usages",
        _ => return None,
    })
}

fn official_origin(provider: &str, base: &str) -> bool {
    let (Some(base), Some(official)) =
        (reqwest::Url::parse(base).ok(), endpoint(provider).and_then(|url| reqwest::Url::parse(url).ok()))
    else {
        return false;
    };
    base.origin() == official.origin()
}

fn query_failed(provider: &str) -> Value {
    json!({"providerId":provider,"status":"query-failed","message":"The provider usage query failed."})
}

fn normalize(provider: &str, p: &Value) -> Option<Value> {
    let captured = now_ms();
    let name = match provider {
        "openai-codex" => "OpenAI Codex",
        "deepseek" => "DeepSeek",
        "openrouter" => "OpenRouter",
        "moonshotai" => "Moonshot AI",
        "moonshotai-cn" => "Moonshot AI CN",
        "minimax" => "MiniMax",
        "minimax-cn" => "MiniMax CN",
        "vercel-ai-gateway" => "Vercel AI Gateway",
        "opencode-go" => "OpenCode Go",
        "kimi-coding" => "Kimi For Coding",
        _ => return None,
    };
    let mut buckets = Vec::<Value>::new();
    let mut metrics = Vec::<Value>::new();
    match provider {
        "openai-codex" => {
            let mut group = |value: &Value, label: &str, id: &str| {
                for (position, raw) in
                    [("primary", get(value, "primary_window")), ("secondary", get(value, "secondary_window"))]
                {
                    let Some(raw) = raw else { continue };
                    let Some(used) = number(get(raw, "used_percent")) else { continue };
                    let seconds = number(get(raw, "limit_window_seconds"));
                    let clamped = clamp(used);
                    let mut bucket = json!({"id":format!("{id}:{position}"),"label":window_label(seconds),"used":clamped,"remaining":100.0-clamped,"limit":100,"unit":"percent"});
                    if id != "codex" {
                        bucket["groupLabel"] = json!(label);
                    }
                    if let Some(seconds) = seconds.filter(|s| *s > 0.0) {
                        bucket["windowMinutes"] = json!((seconds / 60.0).ceil());
                    }
                    if let Some(reset) = number(get(raw, "reset_at")) {
                        bucket["resetsAt"] = json!(reset);
                    }
                    buckets.push(bucket);
                }
            };
            if let Some(rate) = get(p, "rate_limit") {
                group(rate, "Codex", "codex");
            }
            if let Some(items) = get(p, "additional_rate_limits").and_then(Value::as_array) {
                for (i, item) in items.iter().enumerate() {
                    if let Some(rate) = get(item, "rate_limit") {
                        let group_label = clean(get(item, "limit_name")).unwrap_or_else(|| format!("Limit {}", i + 1));
                        let group_id = clean(get(item, "metered_feature")).unwrap_or_else(|| format!("additional-{i}"));
                        group(rate, &group_label, &group_id);
                    }
                }
            }
            if get(p, "credits").and_then(|v| get(v, "unlimited")).and_then(Value::as_bool) == Some(true) {
                metrics.push(json!({"id":"credits","label":"Credits","value":"Unlimited"}));
            } else if let Some(balance) = get(p, "credits").and_then(|v| number(get(v, "balance"))) {
                metrics.push(json!({"id":"credits","label":"Credits","value":balance,"unit":"count"}));
            }
        }
        "deepseek" => {
            metrics.push(json!({"id":"availability","label":"API calls","value":if get(p,"is_available").and_then(Value::as_bool)==Some(true) {"Available"} else {"Unavailable"}}));
            let rows = get(p, "balance_infos").and_then(Value::as_array)?;
            for row in rows {
                let Some(currency) = clean(get(row, "currency")) else { continue };
                if currency != "USD" && currency != "CNY" {
                    continue;
                }
                for (id, label, field) in [
                    ("total", "Total balance", "total_balance"),
                    ("granted", "Granted balance", "granted_balance"),
                    ("topped-up", "Topped-up balance", "topped_up_balance"),
                ] {
                    if let Some(value) = decimal(get(row, field)) {
                        metrics.push(json!({"id":format!("{}-{id}",currency.to_lowercase()),"label":label,"value":value,"unit":"currency","currency":currency}));
                    }
                }
            }
        }
        "openrouter" => {
            let data = get(p, "data")?;
            let limit = nonnegative(get(data, "limit"));
            let remaining = nonnegative(get(data, "limit_remaining"));
            if let Some(limit) = limit {
                let mut b =
                    json!({"id":"key-limit","label":"Key limit","limit":limit,"unit":"currency","currency":"USD"});
                if let Some(r) = remaining {
                    b["remaining"] = json!(r);
                    b["used"] = json!((limit - r).max(0.0));
                }
                if let Some(period) = clean(get(data, "limit_reset")) {
                    b["period"] = json!(period);
                }
                buckets.push(b);
            }
            for (id, label, field) in [
                ("daily", "Usage today", "usage_daily"),
                ("weekly", "Usage this week", "usage_weekly"),
                ("monthly", "Usage this month", "usage_monthly"),
                ("total", "All-time usage", "usage"),
            ] {
                if let Some(value) = nonnegative(get(data, field)) {
                    metrics.push(json!({"id":id,"label":label,"value":value,"unit":"currency","currency":"USD"}));
                }
            }
        }
        "vercel-ai-gateway" => {
            if let Some(v) = decimal(get(p, "balance")) {
                metrics.push(
                    json!({"id":"balance","label":"Credit balance","value":v,"unit":"currency","currency":"USD"}),
                );
            }
            if let Some(v) = decimal(get(p, "total_used")) {
                metrics
                    .push(json!({"id":"spent","label":"Lifetime spend","value":v,"unit":"currency","currency":"USD"}));
            }
        }
        "moonshotai" | "moonshotai-cn" => {
            let data = get(p, "data")?;
            if number(get(p, "code")) != Some(0.0) || get(p, "status").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            let currency = if provider == "moonshotai" { "USD" } else { "CNY" };
            for (id, label, field) in [
                ("available", "Available balance", "available_balance"),
                ("voucher", "Voucher balance", "voucher_balance"),
                ("cash", "Cash balance", "cash_balance"),
            ] {
                if let Some(v) = number(get(data, field)) {
                    metrics.push(
                        json!({"id":id,"label":label,"value":v.to_string(),"unit":"currency","currency":currency}),
                    );
                }
            }
        }
        "minimax" | "minimax-cn" => {
            let currency = if provider == "minimax" { "USD" } else { "CNY" };
            if get(p, "base_resp").and_then(|v| number(get(v, "status_code"))).is_some_and(|c| c != 0.0) {
                return None;
            }
            if let Some(rows) = get(p, "model_remains").and_then(Value::as_array) {
                for (i, row) in rows.iter().enumerate() {
                    let group = clean(get(row, "model_name")).unwrap_or_else(|| format!("Quota {}", i + 1));
                    for (suffix, prefix, start, end) in [
                        ("interval", "current_interval", "start_time", "end_time"),
                        ("weekly", "current_weekly", "weekly_start_time", "weekly_end_time"),
                    ] {
                        let status = number(get(row, &format!("{prefix}_status")));
                        if status == Some(3.0) {
                            buckets.push(json!({"id":format!("{group}:{suffix}"),"label":if suffix=="weekly" {"Weekly"} else {"Rolling"},"groupLabel":group,"remaining":100,"unit":"percent","period":"Unlimited"}));
                            continue;
                        }
                        let pct = nonnegative(get(row, &format!("{prefix}_remaining_percent")));
                        let total = nonnegative(get(row, &format!("{prefix}_total_count")));
                        let count = nonnegative(get(row, &format!("{prefix}_usage_count")));
                        if total.is_none() && pct.is_none() {
                            continue;
                        }
                        let mut b;
                        if let (Some(total), Some(count)) = (total, count) {
                            let mut rem = count.min(total);
                            if let Some(pct) = pct {
                                let as_remaining = rem / total * 100.0;
                                let as_used = (total - rem) / total * 100.0;
                                if (as_used - pct).abs() < (as_remaining - pct).abs() {
                                    rem = total - rem;
                                }
                            }
                            b = json!({"id":format!("{group}:{suffix}"),"label":if suffix=="weekly" {"Weekly"} else {"Rolling"},"groupLabel":group,"used":total-rem,"remaining":rem,"limit":total,"unit":"count"});
                        } else {
                            let rem = clamp(pct.unwrap_or(0.0));
                            b = json!({"id":format!("{group}:{suffix}"),"label":if suffix=="weekly" {"Weekly"} else {"Rolling"},"groupLabel":group,"used":100.0-rem,"remaining":rem,"limit":100,"unit":"percent"});
                        }
                        let st = number(get(row, start));
                        let en = number(get(row, end));
                        if let (Some(st), Some(en)) = (st, en) {
                            b["windowMinutes"] = json!(((en - st) / 60_000.0).round().max(1.0));
                            b["resetsAt"] = json!((en / 1000.0).floor());
                        }
                        buckets.push(b);
                    }
                }
            } else {
                for (id, label, field) in [
                    ("available", "Available balance", "available_amount"),
                    ("cash", "Cash balance", "cash_balance"),
                    ("voucher", "Voucher balance", "voucher_balance"),
                    ("credit", "Credit balance", "credit_balance"),
                ] {
                    if let Some(v) = decimal(get(p, field)) {
                        metrics.push(json!({"id":id,"label":label,"value":v,"unit":"currency","currency":currency}));
                    }
                }
            }
        }
        "opencode-go" => {
            let usage = get(p, "usage").filter(|v| v.is_object()).unwrap_or(p);
            for (id, window) in usage.as_object()? {
                let pct = nonnegative(
                    get(window, "percent")
                        .or_else(|| get(window, "usagePercent"))
                        .or_else(|| get(window, "usage_percent")),
                );
                let Some(pct) = pct else { continue };
                let value = clamp(pct);
                let label = match id.as_str() {
                    "rolling" => "Rolling",
                    "weekly" => "Weekly",
                    "monthly" => "Monthly",
                    _ => id,
                };
                let mut b =
                    json!({"id":id,"label":label,"used":value,"remaining":100.0-value,"limit":100,"unit":"percent"});
                if let Some(v) = epoch(
                    get(window, "resetsAt")
                        .or_else(|| get(window, "resetAt"))
                        .or_else(|| get(window, "resets_at"))
                        .or_else(|| get(window, "reset_at")),
                ) {
                    b["resetsAt"] = json!(v);
                }
                buckets.push(b);
            }
        }
        "kimi-coding" => {
            let mut add = |id: String, label: String, detail: Option<&Value>, seconds: Option<f64>| {
                let Some(detail) = detail else { return };
                let limit = nonnegative(get(detail, "limit"));
                let used = nonnegative(get(detail, "used"));
                let remaining =
                    nonnegative(get(detail, "remaining")).or_else(|| limit.zip(used).map(|(l, u)| (l - u).max(0.0)));
                let (Some(limit), Some(remaining)) = (limit, remaining) else { return };
                let used = used.unwrap_or((limit - remaining).max(0.0));
                let mut b =
                    json!({"id":id,"label":label,"used":used,"remaining":remaining,"limit":limit,"unit":"count"});
                if let Some(s) = seconds {
                    b["windowMinutes"] = json!((s / 60.0).ceil());
                }
                if let Some(t) = epoch(get(detail, "resetTime")) {
                    b["resetsAt"] = json!(t);
                }
                buckets.push(b);
            };
            add("usage".into(), "Overall".into(), get(p, "usage"), None);
            if let Some(limits) = get(p, "limits").and_then(Value::as_array) {
                for (i, v) in limits.iter().enumerate() {
                    let w = get(v, "window");
                    let d = w.and_then(|w| nonnegative(get(w, "duration")));
                    let unit = w.and_then(|w| clean(get(w, "timeUnit")));
                    let mult = match unit.as_deref() {
                        Some("TIME_UNIT_SECOND") => Some(1.0),
                        Some("TIME_UNIT_MINUTE") => Some(60.0),
                        Some("TIME_UNIT_HOUR") => Some(3600.0),
                        Some("TIME_UNIT_DAY") => Some(86400.0),
                        _ => None,
                    };
                    let seconds = d.zip(mult).map(|(d, m)| d * m);
                    add(format!("limit-{i}"), window_label(seconds), get(v, "detail"), seconds);
                }
            }
            if let Some(v) = get(p, "parallel").and_then(|v| nonnegative(get(v, "limit"))) {
                metrics.push(json!({"id":"parallel","label":"Parallel requests","value":v,"unit":"count"}));
            }
            if let Some(v) = get(p, "user").and_then(|u| get(u, "membership")).and_then(|m| clean(get(m, "level"))) {
                metrics.push(
                    json!({"id":"membership","label":"Membership","value":v.strip_prefix("LEVEL_").unwrap_or(&v)}),
                );
            }
        }
        _ => return None,
    }
    if buckets.is_empty() && metrics.is_empty() {
        return None;
    }
    Some(json!({"providerId":provider,"providerName":name,"capturedAt":captured,"buckets":buckets,"metrics":metrics}))
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}
fn nonnegative(value: Option<&Value>) -> Option<f64> {
    number(value).filter(|n| *n >= 0.0)
}
fn decimal(value: Option<&Value>) -> Option<Value> {
    match value? {
        Value::Number(n) => n.as_f64().filter(|n| n.is_finite()).map(|n| json!(n.to_string())),
        Value::String(s) if s.len() <= 64 && s.parse::<f64>().is_ok() => Some(json!(s)),
        _ => None,
    }
}
fn clean(value: Option<&Value>) -> Option<String> {
    value?
        .as_str()
        .map(|s| s.chars().filter(|c| !c.is_control()).take(80).collect::<String>().trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn clamp(n: f64) -> f64 {
    n.clamp(0.0, 100.0)
}
fn window_label(seconds: Option<f64>) -> String {
    let Some(s) = seconds.filter(|s| *s > 0.0) else { return "Limit".into() };
    let m = (s / 60.0).ceil();
    if m >= 10080.0 {
        "Weekly".into()
    } else if m >= 60.0 {
        format!("{}h", (m / 60.0).round())
    } else {
        format!("{m}m")
    }
}
fn epoch(value: Option<&Value>) -> Option<i64> {
    if let Some(n) = number(value) {
        return Some(if n >= 1e12 { (n / 1000.0).floor() as i64 } else { n.floor() as i64 });
    }
    let s = value?.as_str()?;
    let parsed = chrono_free_parse(s)?;
    Some(parsed)
}
fn chrono_free_parse(s: &str) -> Option<i64> {
    let date = s.get(..10)?;
    let mut p = date.split('-');
    let y: i64 = p.next()?.parse().ok()?;
    let m: i64 = p.next()?.parse().ok()?;
    let d: i64 = p.next()?.parse().ok()?;
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let mut result = (era * 146097 + doe - 719468) * 86400;
    if let Some(time) = s.get(10..).filter(|time| time.starts_with(['T', 't', ' '])) {
        let clock = time.get(1..9).unwrap_or("00:00:00");
        let mut parts = clock.split(':');
        let hour: i64 = parts.next()?.parse().ok()?;
        let minute: i64 = parts.next()?.parse().ok()?;
        let second: i64 = parts.next()?.split('.').next()?.parse().ok()?;
        result += hour * 3600 + minute * 60 + second;
        let timezone = time.get(9..).unwrap_or("");
        if let Some(offset_at) = timezone.rfind(['+', '-']) {
            let offset = &timezone[offset_at..];
            let sign = if offset.starts_with('+') { 1 } else { -1 };
            let mut fields = offset[1..].split(':');
            let hours: i64 = fields.next()?.parse().ok()?;
            let minutes: i64 = fields.next().unwrap_or("0").parse().ok()?;
            result -= sign * (hours * 3600 + minutes * 60);
        }
    }
    Some(result)
}
fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}
