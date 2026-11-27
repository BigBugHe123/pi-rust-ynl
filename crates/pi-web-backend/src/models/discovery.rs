use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use pi_coding_agent::agent_session::auth::request_auth;
use pi_coding_agent::agent_session::{ModelRuntime, ModelRuntimeOptions};
use reqwest::Url;
use serde_json::{Map, Value, json};

const PLACEHOLDER_MODEL_ID: &str = "__pi_web_model_discovery__";
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into()}))).into_response()
}

fn clean_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

fn build_models_url(base_url: &str, api: &str) -> Result<Url, ()> {
    let mut url = Url::parse(base_url.trim()).map_err(|_| ())?;
    let trimmed_path = url.path().trim_end_matches('/');
    if !trimmed_path.to_ascii_lowercase().ends_with("/models") {
        let mut path = trimmed_path.to_owned();
        let has_version = path.rsplit('/').next().is_some_and(|part| {
            let part = part.to_ascii_lowercase();
            part.strip_prefix('v').is_some_and(|version| {
                let version = version.strip_suffix("beta").unwrap_or(version);
                !version.is_empty() && version.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
            })
        });
        if api == "anthropic-messages" && !has_version {
            path.push_str("/v1");
        } else if api == "google-generative-ai" && !has_version {
            path.push_str("/v1beta");
        }
        path.push_str("/models");
        url.set_path(&path);
    }
    if api == "anthropic-messages" && url.query_pairs().all(|(key, _)| key != "limit") {
        url.query_pairs_mut().append_pair("limit", "1000");
    }
    if api == "google-generative-ai" && url.query_pairs().all(|(key, _)| key != "pageSize") {
        url.query_pairs_mut().append_pair("pageSize", "1000");
    }
    Ok(url)
}

fn model_headers(value: &Value) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(entries) = value.as_object() {
        for (name, value) in entries {
            let (Ok(name), Some(value)) = (HeaderName::from_bytes(name.as_bytes()), value.as_str()) else { continue };
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        }
    }
    headers
}

fn add_auth_headers(headers: &mut HeaderMap, api: &str, api_key: Option<&str>) {
    headers.entry("accept").or_insert(HeaderValue::from_static("application/json"));
    let Some(api_key) = api_key else { return };
    let Ok(value) = HeaderValue::from_str(api_key) else { return };
    match api {
        "anthropic-messages" => {
            headers.entry("x-api-key").or_insert(value);
            headers.entry("anthropic-version").or_insert(HeaderValue::from_static("2023-06-01"));
        }
        "google-generative-ai" => {
            headers.entry("x-goog-api-key").or_insert(value);
        }
        _ => {
            if !headers.contains_key("authorization") {
                if let Ok(value) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
                    headers.insert("authorization", value);
                }
            }
        }
    }
}

const EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

fn first_positive_integer(value: &Map<String, Value>, fields: &[&str]) -> Option<u64> {
    fields.iter().find_map(|field| {
        let raw = value.get(*field)?;
        let parsed = raw.as_u64().or_else(|| raw.as_str()?.trim().parse::<u64>().ok())?;
        (parsed > 0).then_some(parsed)
    })
}

fn model_from_value(value: &Value) -> Option<Value> {
    if let Some(id) = value.as_str().map(str::trim).filter(|id| !id.is_empty()) {
        return Some(json!({"id":id}));
    }
    let object = value.as_object()?;
    let clean = |field: &str| object.get(field).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let raw_id = clean("id").or_else(|| clean("model")).or_else(|| clean("name"))?;
    let id = raw_id.strip_prefix("models/").unwrap_or(raw_id);
    if id.is_empty() {
        return None;
    }
    let name = clean("display_name")
        .or_else(|| clean("displayName"))
        .or_else(|| (clean("id").is_some() || clean("model").is_some()).then(|| clean("name")).flatten());
    let mut model = Map::new();
    model.insert("id".into(), json!(id));
    if let Some(name) = name.filter(|name| *name != id) {
        model.insert("name".into(), json!(name));
    }
    if ["supports_reasoning", "supportsReasoning", "reasoning", "thinking"]
        .iter()
        .any(|field| object.get(*field) == Some(&Value::Bool(true)))
    {
        model.insert("reasoning".into(), json!(true));
        let effort_values = ["supported_efforts", "supportedEfforts", "reasoning_levels"]
            .iter()
            .find_map(|field| object.get(*field).and_then(Value::as_array));
        if let Some(efforts) = effort_values {
            let advertised = efforts.iter().filter_map(Value::as_str).collect::<std::collections::HashSet<_>>();
            if EFFORTS.iter().any(|effort| advertised.contains(effort)) {
                let mut map = Map::new();
                for effort in EFFORTS {
                    if !advertised.contains(effort) {
                        map.insert(effort.into(), Value::Null);
                    } else if matches!(effort, "xhigh" | "max") {
                        map.insert(effort.into(), json!(effort));
                    }
                }
                model.insert("thinkingLevelMap".into(), Value::Object(map));
            }
        }
    }
    let modalities = object
        .get("modalities")
        .and_then(Value::as_object)
        .and_then(|m| m.get("input"))
        .and_then(Value::as_array)
        .or_else(|| object.get("input").and_then(Value::as_array));
    if let Some(modalities) = modalities {
        let mut input = Vec::new();
        for modality in modalities.iter().filter_map(Value::as_str).map(|s| s.trim().to_ascii_lowercase()) {
            if ["text", "image"].contains(&modality.as_str()) && !input.contains(&modality) {
                input.push(modality);
            }
        }
        if !input.is_empty() {
            model.insert("input".into(), json!(input));
        }
    }
    if let Some(window) = first_positive_integer(
        object,
        &[
            "context_length",
            "contextLength",
            "context_window",
            "contextWindow",
            "max_model_len",
            "max_input_tokens",
            "inputTokenLimit",
        ],
    ) {
        model.insert("contextWindow".into(), json!(window));
    }
    if let Some(tokens) = first_positive_integer(
        object,
        &["max_output_tokens", "maxOutputTokens", "max_completion_tokens", "outputTokenLimit"],
    ) {
        model.insert("maxTokens".into(), json!(tokens));
    }
    Some(Value::Object(model))
}

fn parse_models(value: &Value) -> Vec<Value> {
    let items: Vec<&Value> = if let Some(array) = value.as_array() {
        array.iter().collect()
    } else if let Some(object) = value.as_object() {
        ["data", "models", "results", "items"]
            .iter()
            .find_map(|key| object.get(*key))
            .map(|candidate| {
                candidate
                    .as_array()
                    .map(|array| array.iter().collect())
                    .unwrap_or_else(|| candidate.as_object().map(|o| o.values().collect()).unwrap_or_default())
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut seen = std::collections::HashSet::new();
    let mut models = items
        .into_iter()
        .filter_map(model_from_value)
        .filter(|model| model.get("id").and_then(Value::as_str).is_some_and(|id| seen.insert(id.to_owned())))
        .collect::<Vec<_>>();
    models.sort_by_key(|model| {
        model.get("name").or_else(|| model.get("id")).and_then(Value::as_str).unwrap_or_default().to_lowercase()
    });
    models
}

pub async fn discover(agent_dir: &Path, input: Value) -> Response {
    let Some(provider_name) = clean_string(input.get("providerName")) else {
        return error(StatusCode::BAD_REQUEST, "providerName is required");
    };
    let Some(provider) = input.get("provider").filter(|value| value.is_object()) else {
        return error(StatusCode::BAD_REQUEST, "provider is required");
    };
    let configured_base_url = clean_string(provider.get("baseUrl")).unwrap_or_default();
    let configured_api = clean_string(provider.get("api")).unwrap_or_default();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("pi-web-model-discovery-{}-{stamp}", std::process::id()));
    if let Err(err) = fs::create_dir(&temp_dir) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
    }
    let _temp_dir = TempDir(temp_dir.clone());
    let mut provider_entry = provider.as_object().cloned().unwrap_or_default();
    provider_entry.insert("models".into(), json!([{"id":PLACEHOLDER_MODEL_ID}]));
    let config = json!({"providers": {provider_name.clone(): provider_entry}});
    let models_path = temp_dir.join("models.json");
    if let Err(err) = fs::write(&models_path, serde_json::to_vec_pretty(&config).unwrap_or_default()) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
    }
    let mut options = ModelRuntimeOptions::for_agent_dir(agent_dir);
    options.models_path = models_path;
    let runtime = ModelRuntime::create(options).await;
    if let Some(load_error) = runtime.load_error() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, load_error);
    }
    let Some(model) = runtime.get_model(&provider_name, PLACEHOLDER_MODEL_ID) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, format!("Unable to load provider \"{provider_name}\""));
    };
    let auth = request_auth(&runtime, &model).await.ok();
    let api_key = auth.as_ref().and_then(|auth| auth.auth.api_key.as_deref());
    if clean_string(provider.get("apiKey")).is_some() && api_key.is_none() {
        return error(StatusCode::BAD_REQUEST, format!("No API key found for \"{provider_name}\""));
    }
    let base_url = if !configured_base_url.is_empty() { configured_base_url } else { model.base_url.clone() };
    if base_url.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "Base URL is required");
    }
    let api = if !configured_api.is_empty() {
        configured_api
    } else if !model.api.is_empty() {
        model.api.clone()
    } else {
        "openai-completions".into()
    };
    let endpoint = match build_models_url(&base_url, &api) {
        Ok(url) => url,
        Err(()) => return error(StatusCode::BAD_REQUEST, "Base URL is invalid"),
    };
    let mut headers =
        model_headers(&Value::Object(model.headers.iter().map(|(key, value)| (key.clone(), json!(value))).collect()));
    if let Some(auth) = &auth {
        for (name, value) in &auth.auth.headers {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
                headers.insert(name, value);
            }
        }
    }
    add_auth_headers(&mut headers, &api, api_key);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    let response = match client.get(endpoint.clone()).headers(headers).send().await {
        Ok(response) => response,
        Err(err) if err.is_timeout() => return error(StatusCode::GATEWAY_TIMEOUT, err.to_string()),
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    let upstream_status = response.status();
    let bytes = match response.bytes().await {
        Ok(bytes) if bytes.len() <= MAX_RESPONSE_BYTES => bytes,
        Ok(_) => return error(StatusCode::BAD_GATEWAY, "Upstream model list response was too large"),
        Err(err) => return error(StatusCode::BAD_GATEWAY, err.to_string()),
    };
    if !upstream_status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        let message = if text.is_empty() {
            format!("Upstream returned HTTP {}", upstream_status.as_u16())
        } else {
            text.chars().take(500).collect()
        };
        return (StatusCode::BAD_GATEWAY, Json(json!({"error":message,"status":upstream_status.as_u16()})))
            .into_response();
    }
    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(payload) => payload,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "Upstream model list was not valid JSON"),
    };
    let models = parse_models(&payload);
    if models.is_empty() {
        return error(StatusCode::BAD_GATEWAY, "No models found in the upstream response");
    }
    Json(json!({"models":models,"endpoint":endpoint.as_str()})).into_response()
}
