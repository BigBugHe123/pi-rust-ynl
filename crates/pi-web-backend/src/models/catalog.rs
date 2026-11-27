//! models.dev catalog fetching, caching, flattening and model recommendations.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

const SOURCE: &str = "https://models.dev/api.json";
const TTL: Duration = Duration::from_secs(60 * 60);
const TIMEOUT: Duration = Duration::from_secs(15);
const CONSENSUS_MIN_SHARE: f64 = 0.6;
const CONSENSUS_MIN_SUPPORT: usize = 5;

#[derive(Clone)]
struct Entry {
    value: Value,
    provider_id: String,
    provider_name: String,
    provider_base_url: Option<String>,
    id: String,
    name: String,
    cost_input: Option<f64>,
    cost_output: Option<f64>,
    cost_cache_read: Option<f64>,
    cost_cache_write: Option<f64>,
}

#[derive(Default)]
struct Cache {
    entries: Vec<Entry>,
    expires_at: Option<Instant>,
}

static CACHE: Mutex<Cache> = Mutex::const_new(Cache { entries: Vec::new(), expires_at: None });

struct CatalogQuery {
    q: String,
    provider: String,
    base_url: String,
    limit: i64,
}

pub async fn get(Query(params): Query<std::collections::HashMap<String, String>>) -> Response {
    let query = CatalogQuery {
        q: params.get("q").map(|s| s.chars().take(120).collect()).unwrap_or_default(),
        provider: params.get("provider").map(|s| s.chars().take(120).collect()).unwrap_or_default(),
        base_url: params.get("baseUrl").map(|s| s.chars().take(500).collect()).unwrap_or_default(),
        limit: params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(50),
    };
    match load_catalog().await {
        Ok(entries) => {
            let models = search(&entries, &query);
            let recommendation = recommend(&entries, &query);
            Json(json!({ "models": models, "recommendation": recommendation, "source": SOURCE })).into_response()
        }
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))).into_response(),
    }
}

async fn load_catalog() -> Result<Vec<Entry>, String> {
    let mut cache = CACHE.lock().await;
    if !cache.entries.is_empty() && cache.expires_at.is_some_and(|expiry| expiry > Instant::now()) {
        return Ok(cache.entries.clone());
    }
    let result = async {
        let response = reqwest::Client::new()
            .get(SOURCE)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("models.dev returned HTTP {}", response.status()));
        }
        let raw: Value = response.json().await.map_err(|error| format!("models.dev response parse failed: {error}"))?;
        let entries = flatten(&raw);
        if entries.is_empty() {
            return Err("models.dev returned an empty catalog".to_owned());
        }
        Ok(entries)
    }
    .await;
    match result {
        Ok(entries) => {
            cache.entries = entries.clone();
            cache.expires_at = Some(Instant::now() + TTL);
            Ok(entries)
        }
        Err(_error) if !cache.entries.is_empty() => Ok(cache.entries.clone()),
        Err(error) => Err(error),
    }
}

fn object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}
fn clean_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}
fn nonnegative(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0)
}
fn positive(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite() && *n > 0.0)
}

fn flatten(raw: &Value) -> Vec<Entry> {
    let mut entries = Vec::new();
    let Some(providers) = object(raw) else { return entries };
    for (provider_id, provider) in providers {
        let Some(provider) = object(provider) else { continue };
        let Some(models) = provider.get("models").and_then(Value::as_object) else { continue };
        let provider_name = clean_string(provider.get("name")).unwrap_or_else(|| provider_id.clone());
        let provider_base_url = clean_string(provider.get("api"));
        for (fallback_id, model) in models {
            let Some(model) = object(model) else { continue };
            let id = clean_string(model.get("id")).unwrap_or_else(|| fallback_id.clone());
            if id.is_empty() {
                continue;
            }
            let name = clean_string(model.get("name")).unwrap_or_else(|| id.clone());
            let cost = model.get("cost").and_then(Value::as_object);
            let modalities = model
                .get("modalities")
                .and_then(Value::as_object)
                .and_then(|m| m.get("input"))
                .and_then(Value::as_array);
            let input: Vec<String> = modalities
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| s == "text" || s == "image")
                .fold(Vec::new(), |mut values, s| {
                    if !values.contains(&s) {
                        values.push(s);
                    }
                    values
                });
            let limit = model.get("limit").and_then(Value::as_object);
            let mut normalized = Map::new();
            normalized.insert("key".into(), json!(format!("{provider_id}/{id}")));
            normalized.insert("providerId".into(), json!(provider_id));
            normalized.insert("providerName".into(), json!(provider_name.clone()));
            if let Some(url) = &provider_base_url {
                normalized.insert("providerBaseUrl".into(), json!(url));
            }
            normalized.insert("id".into(), json!(id.clone()));
            normalized.insert("name".into(), json!(name.clone()));
            if let Some(reasoning) = model.get("reasoning").and_then(Value::as_bool) {
                normalized.insert("reasoning".into(), json!(reasoning));
            }
            if !input.is_empty() {
                normalized.insert("input".into(), json!(input));
            }
            if let Some(n) = limit.and_then(|l| positive(l.get("context"))) {
                normalized.insert("contextWindow".into(), json!(n));
            }
            if let Some(n) = limit.and_then(|l| positive(l.get("output"))) {
                normalized.insert("maxTokens".into(), json!(n));
            }
            let mut normalized_cost = Map::new();
            if let Some(n) = cost.and_then(|c| nonnegative(c.get("input"))) {
                normalized_cost.insert("input".into(), json!(n));
            }
            if let Some(n) = cost.and_then(|c| nonnegative(c.get("output"))) {
                normalized_cost.insert("output".into(), json!(n));
            }
            if let Some(n) = cost.and_then(|c| nonnegative(c.get("cache_read"))) {
                normalized_cost.insert("cacheRead".into(), json!(n));
            }
            if let Some(n) = cost.and_then(|c| nonnegative(c.get("cache_write"))) {
                normalized_cost.insert("cacheWrite".into(), json!(n));
            }
            normalized.insert("cost".into(), Value::Object(normalized_cost));
            let value = Value::Object(normalized);
            let cost = value.get("cost").and_then(Value::as_object);
            let cost_input = cost.and_then(|c| c.get("input")).and_then(Value::as_f64);
            let cost_output = cost.and_then(|c| c.get("output")).and_then(Value::as_f64);
            let cost_cache_read = cost.and_then(|c| c.get("cacheRead")).and_then(Value::as_f64);
            let cost_cache_write = cost.and_then(|c| c.get("cacheWrite")).and_then(Value::as_f64);
            entries.push(Entry {
                provider_id: provider_id.clone(),
                provider_name: provider_name.clone(),
                provider_base_url: provider_base_url.clone(),
                id,
                name,
                value,
                cost_input,
                cost_output,
                cost_cache_read,
                cost_cache_write,
            });
        }
    }
    entries
}

fn normalize_provider(value: &str) -> String {
    value.trim().to_ascii_lowercase().chars().filter(char::is_ascii_alphanumeric).collect()
}
fn normalize_id(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    normalized.strip_prefix("models/").unwrap_or(&normalized).to_owned()
}
fn hostname(value: &str) -> Option<String> {
    url::Url::parse(value).ok()?.host_str().map(|s| s.trim_end_matches('.').to_ascii_lowercase())
}
fn host_matches(actual: &str, expected: &str) -> bool {
    actual == expected || actual.strip_suffix(expected).is_some_and(|prefix| prefix.ends_with('.'))
}
fn provider_matches(entry: &Entry, hint: &str) -> bool {
    let hint = normalize_provider(hint);
    !hint.is_empty()
        && (normalize_provider(&entry.provider_id) == hint || normalize_provider(&entry.provider_name) == hint)
}
fn base_url_matches(entry: &Entry, url: &str) -> bool {
    let Some(actual) = hostname(url) else { return false };
    let normalized = normalize_provider(&entry.provider_id);
    let known = match normalized.as_str() {
        "anthropic" => Some("api.anthropic.com"),
        "google" => Some("generativelanguage.googleapis.com"),
        "openai" => Some("api.openai.com"),
        "openrouter" => Some("openrouter.ai"),
        _ => None,
    };
    [known.map(str::to_owned), entry.provider_base_url.as_deref().and_then(hostname)]
        .into_iter()
        .flatten()
        .any(|expected| host_matches(&actual, &expected))
}
fn exact_match(entry: &Entry, query: &str) -> bool {
    let q = normalize_id(query);
    !q.is_empty() && (normalize_id(&entry.id) == q || format!("{}/{q}", entry.provider_id.to_ascii_lowercase()) == q)
}
fn has_price(entry: &Entry) -> bool {
    entry.cost_input.is_some() && entry.cost_output.is_some()
}

fn search(entries: &[Entry], query: &CatalogQuery) -> Vec<Value> {
    let q = query.q.trim().to_ascii_lowercase();
    let provider = query.provider.trim().to_ascii_lowercase();
    let limit = query.limit.clamp(1, 100) as usize;
    let mut ranked: Vec<(f64, &Entry)> = entries
        .iter()
        .filter_map(|entry| {
            let id = entry.id.to_ascii_lowercase();
            let name = entry.name.to_ascii_lowercase();
            let pid = entry.provider_id.to_ascii_lowercase();
            let pname = entry.provider_name.to_ascii_lowercase();
            let full = format!("{pid}/{id}");
            let rank = if q.is_empty() {
                10.0
            } else if id == q || full == q {
                0.0
            } else if name == q {
                1.0
            } else if id.starts_with(&q) || name.starts_with(&q) {
                2.0
            } else if full.starts_with(&q) || pid == q || pname == q {
                3.0
            } else if id.contains(&q) || name.contains(&q) {
                4.0
            } else if full.contains(&q) || pname.contains(&q) {
                5.0
            } else {
                return None;
            };
            let rank = if rank < 20.0 && !provider.is_empty() && (pid == provider || pname == provider) {
                rank - 0.5
            } else {
                rank
            };
            Some((rank, entry))
        })
        .collect();
    ranked.sort_by(|(ra, a), (rb, b)| {
        ra.partial_cmp(rb)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.provider_name.to_lowercase().cmp(&b.provider_name.to_lowercase()))
            .then_with(|| natural_cmp(&a.name, &b.name))
            .then_with(|| natural_cmp(&a.id, &b.id))
    });
    ranked.into_iter().take(limit).map(|(_, entry)| entry.value.clone()).collect()
}

/// Case-insensitive natural ordering for model names and IDs. This mirrors
/// `localeCompare(..., { numeric: true, sensitivity: "base" })` for the
/// ASCII digit runs commonly used in model version names (for example,
/// `model-2` sorts before `model-10`).
fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    let left = left.as_bytes();
    let right = right.as_bytes();
    let (mut i, mut j) = (0, 0);

    while i < left.len() && j < right.len() {
        if left[i].is_ascii_digit() && right[j].is_ascii_digit() {
            let (start_i, start_j) = (i, j);
            while i < left.len() && left[i].is_ascii_digit() {
                i += 1;
            }
            while j < right.len() && right[j].is_ascii_digit() {
                j += 1;
            }
            let run_left = &left[start_i..i];
            let run_right = &right[start_j..j];
            let significant_left = run_left
                .iter()
                .position(|byte| *byte != b'0')
                .map_or(&run_left[run_left.len()..], |start| &run_left[start..]);
            let significant_right = run_right
                .iter()
                .position(|byte| *byte != b'0')
                .map_or(&run_right[run_right.len()..], |start| &run_right[start..]);
            let ordering = significant_left
                .len()
                .cmp(&significant_right.len())
                .then_with(|| significant_left.cmp(significant_right))
                .then_with(|| run_left.len().cmp(&run_right.len()));
            if ordering != Ordering::Equal {
                return ordering;
            }
        } else {
            let ordering = left[i].cmp(&right[j]);
            if ordering != Ordering::Equal {
                return ordering;
            }
            i += 1;
            j += 1;
        }
    }
    left.len().cmp(&right.len())
}

fn mode_string(values: impl IntoIterator<Item = String>, total: usize, case_insensitive: bool) -> Option<String> {
    let mut groups: std::collections::HashMap<String, (String, usize)> = std::collections::HashMap::new();
    for value in values {
        let key = if case_insensitive { value.to_lowercase() } else { value.clone() };
        let group = groups.entry(key).or_insert((value, 0));
        group.1 += 1;
    }
    let mut ranked: Vec<_> = groups.into_values().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let winner = ranked.first()?;
    if winner.1 as f64 / (total.max(1) as f64) < CONSENSUS_MIN_SHARE
        || ranked.get(1).is_some_and(|second| second.1 == winner.1)
    {
        None
    } else {
        Some(winner.0.clone())
    }
}
fn mode_number(values: &[f64]) -> Option<f64> {
    let mut groups: Vec<(f64, usize)> = Vec::new();
    for value in values {
        if let Some(group) = groups.iter_mut().find(|(candidate, _)| candidate.to_bits() == value.to_bits()) {
            group.1 += 1;
        } else {
            groups.push((*value, 1));
        }
    }
    groups.sort_by(|a, b| b.1.cmp(&a.1));
    if groups.first().is_some_and(|winner| groups.get(1).is_none_or(|second| second.1 != winner.1)) {
        groups.first().map(|g| g.0)
    } else {
        None
    }
}
fn mode_number_with_total(values: &[f64], total: usize) -> Option<f64> {
    let winner = mode_number(values)?;
    let support = values.iter().filter(|value| value.to_bits() == winner.to_bits()).count();
    if support as f64 / total.max(1) as f64 >= CONSENSUS_MIN_SHARE { Some(winner) } else { None }
}
fn mode_array(values: Vec<Vec<String>>, total: usize) -> Option<Vec<String>> {
    let mut groups: std::collections::HashMap<String, (Vec<String>, usize)> = std::collections::HashMap::new();
    for value in values {
        let mut sorted = value.clone();
        sorted.sort();
        let key = sorted.join(",");
        let group = groups.entry(key).or_insert((value, 0));
        group.1 += 1;
    }
    let mut ranked = groups.into_values().collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let winner = ranked.first()?;
    if winner.1 as f64 / (total.max(1) as f64) < CONSENSUS_MIN_SHARE
        || ranked.get(1).is_some_and(|second| second.1 == winner.1)
    {
        None
    } else {
        Some(winner.0.clone())
    }
}
fn metadata(entry: &Entry) -> Value {
    let mut result = Map::new();
    for key in ["name", "reasoning", "input", "contextWindow", "maxTokens"] {
        if let Some(value) = entry.value.get(key) {
            result.insert(key.into(), value.clone());
        }
    }
    Value::Object(result)
}
fn consensus_metadata(entries: &[&Entry]) -> Value {
    let total = entries.len();
    let mut out = Map::new();
    for (key, values, ci) in [
        ("name", entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), true),
        ("reasoning", entries.iter().filter_map(|e| e.value.get("reasoning").map(Value::to_string)).collect(), false),
    ] {
        if let Some(value) = mode_string(values, total, ci) {
            if key == "reasoning" {
                out.insert(key.into(), Value::Bool(value == "true"));
            } else {
                out.insert(key.into(), json!(value));
            }
        }
    }
    for key in ["contextWindow", "maxTokens"] {
        let values = entries.iter().filter_map(|e| e.value.get(key).and_then(Value::as_f64)).collect::<Vec<_>>();
        if let Some(n) = mode_number_with_total(&values, total) {
            out.insert(key.into(), json!(n));
        }
    }
    let inputs = entries
        .iter()
        .filter_map(|e| e.value.get("input").and_then(Value::as_array))
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    if let Some(winner) = mode_array(inputs, total) {
        if !winner.is_empty() {
            out.insert("input".into(), json!(winner));
        }
    }
    Value::Object(out)
}
fn price_entry(entry: &Entry, method: &str) -> Value {
    json!({"status":"reliable","method":method,"cost":{"input":entry.cost_input,"output":entry.cost_output,"cacheRead":entry.cost_cache_read.unwrap_or(0.0),"cacheWrite":entry.cost_cache_write.unwrap_or(0.0)},"providerId":entry.provider_id,"providerName":entry.provider_name,"support":1,"total":1})
}
fn consensus_price(entries: &[&Entry]) -> Value {
    let priced = entries.iter().filter(|e| has_price(e)).copied().collect::<Vec<_>>();
    if priced.is_empty() {
        return json!({"status":"unreliable","reason":"no-valid-price","support":0,"total":0});
    }
    if priced.len() == 1 {
        return json!({"status":"unreliable","reason":"insufficient-support","support":1,"total":1});
    }
    let mut groups: Vec<(f64, f64, Vec<&Entry>)> = Vec::new();
    for e in &priced {
        let pair = (e.cost_input.unwrap(), e.cost_output.unwrap());
        if let Some(g) =
            groups.iter_mut().find(|g| g.0.to_bits() == pair.0.to_bits() && g.1.to_bits() == pair.1.to_bits())
        {
            g.2.push(e)
        } else {
            groups.push((pair.0, pair.1, vec![e]));
        }
    }
    groups.sort_by(|a, b| b.2.len().cmp(&a.2.len()));
    let winner = &groups[0];
    let support = winner.2.len();
    if groups.get(1).is_some_and(|g| g.2.len() == support)
        || (support as f64 / (priced.len() as f64) < CONSENSUS_MIN_SHARE && support < CONSENSUS_MIN_SUPPORT)
    {
        return json!({"status":"unreliable","reason":"conflict","support":support,"total":priced.len()});
    }
    let reads = winner.2.iter().map(|e| e.cost_cache_read.unwrap_or(0.0)).collect::<Vec<_>>();
    let writes = winner.2.iter().map(|e| e.cost_cache_write.unwrap_or(0.0)).collect::<Vec<_>>();
    json!({"status":"reliable","method":"consensus","cost":{"input":winner.0,"output":winner.1,"cacheRead":mode_number(&reads).unwrap_or(0.0),"cacheWrite":mode_number(&writes).unwrap_or(0.0)},"support":support,"total":priced.len()})
}
fn recommend(entries: &[Entry], query: &CatalogQuery) -> Value {
    let exact = entries.iter().filter(|e| exact_match(e, &query.q)).collect::<Vec<_>>();
    if exact.is_empty() {
        return json!({"exactMatches":0,"metadataMethod":"none","preset":{},"price":{"status":"unreliable","reason":"no-exact-match","support":0,"total":0}});
    }
    let providers = exact.iter().copied().filter(|e| provider_matches(e, &query.provider)).collect::<Vec<_>>();
    let bases = exact.iter().copied().filter(|e| base_url_matches(e, &query.base_url)).collect::<Vec<_>>();
    let metadata_entry = providers.first().copied().or_else(|| bases.first().copied());
    let method = if !providers.is_empty() {
        "provider"
    } else if !bases.is_empty() {
        "base-url"
    } else {
        "consensus"
    };
    let mut preset = metadata_entry.map(metadata).unwrap_or_else(|| consensus_metadata(&exact));
    let price = providers
        .iter()
        .find(|e| has_price(e))
        .map(|e| price_entry(e, "provider"))
        .or_else(|| bases.iter().find(|e| has_price(e)).map(|e| price_entry(e, "base-url")))
        .unwrap_or_else(|| consensus_price(&exact));
    if price["status"] == "reliable" {
        if let Some(p) = preset.as_object_mut() {
            p.insert("cost".into(), price["cost"].clone());
        }
    }
    let mut result = json!({"exactMatches":exact.len(),"metadataMethod":method,"preset":preset,"price":price});
    if let Some(entry) = metadata_entry {
        result["matchedProviderId"] = json!(entry.provider_id);
        result["matchedProviderName"] = json!(entry.provider_name);
    }
    result
}
