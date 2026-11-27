use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::Json;
use axum::extract::Request;
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use futures::stream;
use pi_ai::auth::{AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult, AuthType, LoginOptions};
use pi_ai::registry::CacheRetention;
use pi_ai::{Context, Message, StreamOptions, UserContent, UserMessage};
use pi_coding_agent::agent_session::auth::request_auth;
use pi_coding_agent::agent_session::{ModelRuntime, ModelRuntimeOptions};
use pi_coding_agent::config::agent_dir;
use pi_coding_agent::model_resolver::{ModelScopeCode, resolve_model_scope};
use pi_coding_agent::session::listing::list_all_sessions;
use pi_coding_agent::session::{SessionInfo, SessionManager};
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod api_compat;
mod app_state;
mod app_update;
mod bash_output;
mod cwd;
mod default_cwd;
mod default_preferences;
mod file_index;
mod git_changes;
mod http;
mod mcp;
mod models;
mod open_in_explorer;
mod path_access;
mod plugin_inventory;
mod plugin_updates;
mod plugins;
mod project_trust;
mod prompts;
mod provider_auth;
mod provider_usage;
mod push;
mod rpc_worker;
mod session_context;
mod session_file_reference;
mod session_notices;
mod session_search;
mod session_tree_projection;
mod session_ui_state;
mod sessions;
mod skills;
mod static_files;
mod subagent_profiles;
mod subagent_settings;
mod terminal;
mod tool_settings;
mod web_auth;
mod web_ui;
mod workspace_files;
mod worktrees;

use app_state::{AppState, failure, lock, ok};
use web_ui::WebExtensionUi;

fn resolve_models_cwd(state: &AppState, requested: Option<&str>) -> Result<PathBuf, Response> {
    let raw = requested
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let candidate = if raw.is_absolute() { raw } else { std::env::current_dir().unwrap_or_default().join(raw) };

    let resolved = if candidate.is_dir() {
        candidate.canonicalize().ok()
    } else if !candidate.exists() {
        path_access::infer_removed_worktree_repo(&candidate)
    } else {
        return Err(failure(StatusCode::BAD_REQUEST, format!("Not a directory: {}", candidate.display())));
    };

    let Some(resolved) = resolved else {
        let cwd = candidate.to_string_lossy().to_string();
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Directory does not exist: {cwd}"),
                "reason": "cwd-missing",
                "cwd": cwd,
            })),
        )
            .into_response());
    };
    if !resolved.is_dir() {
        return Err(failure(StatusCode::BAD_REQUEST, format!("Not a directory: {}", resolved.display())));
    }
    let Some(authorized) = path_access::allowed_existing_path(state, &resolved) else {
        return Err(failure(StatusCode::FORBIDDEN, "Access denied"));
    };
    Ok(authorized)
}

async fn models(State(state): State<AppState>, Query(mut query): Query<HashMap<String, String>>) -> Response {
    let cwd = match resolve_models_cwd(&state, query.get("cwd").map(String::as_str)) {
        Ok(cwd) => cwd,
        Err(response) => return response,
    };
    query.insert("cwd".to_string(), cwd.to_string_lossy().to_string());
    let mut query_parts = query.iter().map(|(key, value)| (key, value)).collect::<Vec<_>>();
    query_parts.sort_by(|left, right| left.0.cmp(right.0));
    let cache_key = serde_json::to_string(&(cwd.to_string_lossy(), query_parts)).unwrap_or_default();
    if let Some(value) = models::cache::get(&cache_key) {
        return Json(value).into_response();
    }
    let _key_guard = models::cache::lock_key(&cache_key).await;
    if let Some(value) = models::cache::get(&cache_key) {
        return Json(value).into_response();
    }
    let generation = models::cache::generation();
    let response = models_uncached(State(state), Query(query)).await;
    if !response.status().is_success() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Model catalog response read failed: {error}")),
    };
    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        models::cache::insert(cache_key, value, generation);
    }
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

async fn models_uncached(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Response {
    let cwd = query
        .get("cwd")
        .map(|value| PathBuf::from(value))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", cwd.display()));
    }

    // Model and auth discovery do not require an AgentSession or extension execution runtime.
    let model_runtime = ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&state.agent_dir)).await;
    let project_trusted = match project_trust::resources_trusted(&cwd, &state.agent_dir) {
        Ok(trusted) => trusted,
        Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let settings = SettingsManager::create(&cwd, &state.agent_dir, SettingsOptions { project_trusted });
    // Match the selector/editor catalog: only chat models whose providers have
    // configured auth are available; signed-out providers remain stale entries.
    let all_models = model_runtime.available_models();
    let scope_all = query.get("scope").is_some_and(|value| value == "all");
    let patterns = if scope_all { None } else { settings.enabled_models() };
    let (scoped, diagnostics) = patterns
        .as_deref()
        .filter(|patterns| !patterns.is_empty())
        .map(|patterns| resolve_model_scope(patterns, &all_models))
        .unwrap_or_default();
    let is_scoped = !scoped.is_empty();
    let visible = if is_scoped {
        scoped.iter().map(|item| item.model.clone()).collect::<Vec<_>>()
    } else {
        let mut models = all_models.clone();
        models.sort_by(|left, right| left.provider.cmp(&right.provider));
        models
    };

    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.code != ModelScopeCode::NoMatch
                || scoped.is_empty()
                || !is_suppressible_model_glob(&diagnostic.pattern)
        })
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    let provider_names = model_runtime.all_providers().into_iter().collect::<HashMap<_, _>>();
    let mut names = Map::new();
    let mut model_list = Vec::new();
    let mut thinking_levels = Map::new();
    let mut thinking_level_maps = Map::new();
    let builtin_providers = pi_ai::models::builtin_models()
        .iter()
        .map(|model| model.provider.as_str())
        .collect::<std::collections::HashSet<_>>();
    let custom_providers = all_models
        .iter()
        .map(|model| model.provider.as_str())
        .filter(|provider| !builtin_providers.contains(provider))
        .collect::<std::collections::HashSet<_>>();
    for model in &visible {
        let key = format!("{}:{}", model.provider, model.id);
        names.insert(key.clone(), json!(model.name));
        model_list.push(json!({
            "id": model.id,
            "name": model.name,
            "provider": model.provider,
            "providerName": provider_names.get(&model.provider).unwrap_or(&model.provider),
            "input": model.input,
            "contextWindow": model.context_window,
        }));
        thinking_levels.insert(
            key.clone(),
            json!(
                pi_ai::models::supported_thinking_levels(model).iter().map(|level| level.as_str()).collect::<Vec<_>>()
            ),
        );
        if let Some(levels) = &model.thinking_level_map {
            thinking_level_maps.insert(key, json!(levels));
        }
    }

    let default_ref = settings.default_provider().zip(settings.default_model());
    let default_model = default_ref
        .as_ref()
        .and_then(|(provider, id)| visible.iter().find(|model| model.provider == *provider && model.id == *id))
        .or_else(|| if is_scoped { visible.first() } else { None });
    let default = default_model.map(|model| json!({"provider":model.provider, "modelId":model.id}));
    let pinned_level = default_model.and_then(|model| {
        scoped
            .iter()
            .find(|item| item.model.provider == model.provider && item.model.id == model.id)
            .and_then(|item| item.thinking_level)
    });
    let default_thinking_level = pinned_level
        .or_else(|| default_model.and_then(|model| settings.model_thinking_level(&model.provider, &model.id)))
        .or_else(|| settings.default_thinking_level());
    let model_error = model_runtime.load_error().map(str::to_string);
    let model_data = query.get("enabledPatterns").map(|value| {
        serde_json::from_str::<Vec<String>>(value).map(|patterns| {
            patterns
                .iter()
                .map(|pattern| {
                    let (matched, _) = resolve_model_scope(std::slice::from_ref(pattern), &all_models);
                    let pin = matched.iter().find_map(|item| item.thinking_level.map(|level| level.as_str()));
                    json!({
                        "pattern": pattern,
                        "matched": matched.iter().map(|item| format!("{}/{}", item.model.provider, item.model.id)).collect::<Vec<_>>(),
                        "pin": pin,
                    })
                })
                .collect::<Vec<_>>()
        })
    });
    let model_data = match model_data {
        Some(Ok(data)) => Some(data),
        Some(Err(error)) => return failure(StatusCode::BAD_REQUEST, format!("Invalid enabledPatterns: {error}")),
        None => None,
    };
    let provider_globs = if query.get("withProviderGlobs").is_some_and(|value| value == "1") {
        let providers =
            all_models.iter().map(|model| model.provider.as_str()).collect::<std::collections::BTreeSet<_>>();
        let mut globs = Map::new();
        for provider in providers {
            let expected = all_models
                .iter()
                .filter(|model| model.provider == provider)
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<std::collections::HashSet<_>>();
            for suffix in ["*", "**"] {
                let candidate = format!("{provider}/{suffix}");
                let (matched, _) = resolve_model_scope(std::slice::from_ref(&candidate), &all_models);
                let refs = matched
                    .iter()
                    .map(|item| format!("{}/{}", item.model.provider, item.model.id))
                    .collect::<std::collections::HashSet<_>>();
                if refs == expected {
                    globs.insert(provider.to_string(), json!(candidate));
                    break;
                }
            }
        }
        Some(globs)
    } else {
        None
    };
    let mut data = json!({
        "models": names,
        "modelList": model_list,
        "defaultModel": default,
        "defaultThinkingLevel": default_thinking_level.map(|level| level.as_str()),
        "savedDefaultThinkingLevel": settings.default_thinking_level().map(|level| level.as_str()),
        "thinkingLevels": thinking_levels,
        "thinkingLevelMaps": thinking_level_maps,
        "customProviders": custom_providers,
        "thinkingLevelPins": scoped.iter().filter_map(|item| item.thinking_level.map(|level| (
            format!("{}/{}", item.model.provider, item.model.id), level.as_str()
        ))).collect::<HashMap<_, _>>(),
        "isScoped": is_scoped,
    });
    if let Some(model_data) = model_data {
        data["patternResolutions"] = json!(model_data);
    }
    if let Some(provider_globs) = provider_globs {
        data["providerGlobs"] = json!(provider_globs);
    }
    if !warnings.is_empty() {
        data["modelScopeWarnings"] = json!(warnings);
    }
    if let Some(error) = model_error {
        data["modelError"] = json!(error);
    }
    (StatusCode::OK, Json(data)).into_response()
}

async fn refresh_models(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    if std::env::var_os("PI_OFFLINE").is_some() {
        return Json(json!({"completed":false,"changed":false,"reason":"offline"})).into_response();
    }
    let provider = input.get("provider").and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty());
    let model_runtime = ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&state.agent_dir)).await;
    let signature = |models: Vec<pi_ai::Model>| {
        let mut models = models
            .into_iter()
            .map(|model| format!("{}\0{}\0{}", model.provider, model.id, model.name))
            .collect::<Vec<_>>();
        models.sort();
        models
    };
    let before = signature(model_runtime.all_models());
    let refresh = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        model_runtime.refresh_catalogs(true, provider.map(|value| vec![value.to_owned()]), true),
    )
    .await;
    let changed = signature(model_runtime.all_models()) != before;
    let result = match refresh {
        Ok(result) => json!({"completed":!result.aborted && result.errors.is_empty(),"changed":changed}),
        Err(_) => json!({"completed":false,"changed":changed,"reason":"runtime"}),
    };
    if changed {
        models::cache::invalidate();
    }
    Json(result).into_response()
}

async fn invalidate_models_cache() -> Response {
    models::cache::invalidate();
    Json(json!({"ok":true})).into_response()
}

const MODEL_TEST_TIMEOUT_MS: u64 = 20_000;

struct ModelTestTempDir(PathBuf);

impl Drop for ModelTestTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn test_model(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let Some(provider_name) =
        input.get("providerName").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok":false,"error":"providerName is required"}))).into_response();
    };
    let Some(provider) = input.get("provider").filter(|value| value.is_object()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok":false,"error":"provider is required"}))).into_response();
    };
    let Some(model_input) = input.get("model").and_then(Value::as_object) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok":false,"error":"model is required"}))).into_response();
    };
    let model_id = model_input.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let Some(model_id) = model_id else {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok":false,"error":"Model ID is required"}))).into_response();
    };

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("pi-web-model-test-{}-{stamp}", std::process::id()));
    if let Err(error) = fs::create_dir(&temp_dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok":false,"error":error.to_string()})))
            .into_response();
    }
    let _temp_dir = ModelTestTempDir(temp_dir.clone());
    let models_path = temp_dir.join("models.json");
    let mut model = Value::Object(model_input.clone());
    model["id"] = Value::String(model_id.to_owned());
    // Pi's models.json provider fields sit alongside `models`; the request body uses
    // that same provider object, so flatten it into the provider entry.
    let mut provider_entry = provider.as_object().cloned().unwrap_or_default();
    provider_entry.insert("models".into(), json!([model]));
    let config = json!({"providers": {provider_name: provider_entry}});
    if let Err(error) = fs::write(&models_path, serde_json::to_vec_pretty(&config).unwrap_or_default()) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok":false,"error":error.to_string()})))
            .into_response();
    }

    let mut runtime_options = ModelRuntimeOptions::for_agent_dir(&state.agent_dir);
    runtime_options.models_path = models_path;
    let runtime = ModelRuntime::create(runtime_options).await;
    if let Some(error) = runtime.load_error() {
        return Json(json!({"ok":false,"error":error})).into_response();
    }
    let Some(model) = runtime.get_model(provider_name, model_id) else {
        return Json(json!({"ok":false,"error":format!("Model not found: {provider_name}/{model_id}")}))
            .into_response();
    };
    let auth = match request_auth(&runtime, &model).await {
        Ok(auth) => auth,
        Err(error) => return Json(json!({"ok":false,"error":error})).into_response(),
    };

    let status = Arc::new(AtomicU16::new(0));
    let response_status = status.clone();
    let session_id = format!(
        "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
        (stamp >> 96) as u32,
        (stamp >> 80) as u16,
        (stamp >> 68) as u16 & 0x0fff,
        (stamp >> 56) as u16 & 0x0fff,
        stamp as u64 & 0x0000_ffff_ffff_ffff,
    );
    let mut stream_options = StreamOptions {
        api_key: auth.auth.api_key.clone(),
        headers: auth.auth.headers.clone(),
        env: auth.env.unwrap_or_default(),
        session_id: Some(session_id.clone()),
        max_tokens: Some(16),
        timeout_ms: Some(MODEL_TEST_TIMEOUT_MS),
        max_retries: Some(0),
        max_retry_delay_ms: Some(0),
        cache_retention: Some(CacheRetention::None),
        on_response: Some(Arc::new(move |response, _| response_status.store(response.status, Ordering::Relaxed))),
        ..StreamOptions::default()
    };
    if let Some((name, value)) = pi_ai::identity::opencode_session_header(&auth.model, Some(&session_id)) {
        stream_options.headers.entry(name.to_owned()).or_insert(value);
    }

    let started = std::time::Instant::now();
    let context = Context {
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text("Reply with OK only.".into()),
            timestamp: pi_ai::types::now_ms(),
        })],
        ..Context::default()
    };
    let completion = tokio::time::timeout(
        std::time::Duration::from_millis(MODEL_TEST_TIMEOUT_MS),
        pi_ai::complete(&auth.model, &context, stream_options),
    )
    .await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let response_status = match status.load(Ordering::Relaxed) {
        0 => None,
        value => Some(value),
    };
    let Some(message) = completion.ok() else {
        return Json(json!({"ok":false,"error":"Test timed out","latencyMs":latency_ms,"status":response_status}))
            .into_response();
    };
    if matches!(message.stop_reason, pi_ai::StopReason::Error | pi_ai::StopReason::Aborted) {
        let error = message.error_message.unwrap_or_else(|| "Model returned an error".into());
        return Json(json!({"ok":false,"error":error,"latencyMs":latency_ms,"status":response_status})).into_response();
    }
    let response_text = message
        .content
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let response_text = response_text.chars().take(300).collect::<String>();
    Json(json!({"ok":true,"latencyMs":latency_ms,"status":response_status,"responseText":response_text}))
        .into_response()
}

async fn discover_models(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    models::discovery::discover(&state.agent_dir, input).await
}

fn authorize_existing_workspace(state: &AppState, raw: &str) -> Result<PathBuf, Response> {
    let Some(path) = path_access::allowed_existing_path(state, Path::new(raw)) else {
        return Err(failure(StatusCode::FORBIDDEN, "Access denied"));
    };
    Ok(path)
}

fn authorize_json_workspace(state: &AppState, input: &mut Value) -> Result<(), Response> {
    let Some(raw) = input.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty()) else {
        return Ok(());
    };
    let path = authorize_existing_workspace(state, raw)?;
    if let Some(object) = input.as_object_mut() {
        object.insert("cwd".to_owned(), json!(path.to_string_lossy()));
    }
    Ok(())
}

async fn save_default_preferences(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    default_preferences::save(&state, input).await
}

async fn get_enabled_model_settings(State(state): State<AppState>, query: Query<HashMap<String, String>>) -> Response {
    models::enabled_settings::get(State(state), query).await
}

async fn put_enabled_model_settings(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    models::enabled_settings::put(&state, input)
}

async fn list_skills(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Response {
    let Some(cwd) = query.get("cwd") else { return failure(StatusCode::BAD_REQUEST, "cwd required") };
    let Ok(cwd) = authorize_existing_workspace(&state, cwd) else {
        return failure(StatusCode::FORBIDDEN, "Access denied");
    };
    skills::list(&state.agent_dir, &cwd).await
}

async fn toggle_skills(State(state): State<AppState>, Json(mut input): Json<Value>) -> Response {
    let mut roots = path_access::allowed_roots(&state);
    roots.push(state.agent_dir.clone());
    roots.push(pi_coding_agent::resources::paths::home_dir().join(".agents").join("skills"));
    let roots = roots.into_iter().map(|path| Value::String(path.to_string_lossy().into_owned())).collect::<Vec<_>>();
    if let Some(object) = input.as_object_mut() {
        object.insert("allowedRoots".to_owned(), Value::Array(roots));
    }
    skills::toggle(input)
}

async fn skill_content(State(state): State<AppState>, Query(mut query): Query<skills::ContentQuery>) -> Response {
    let Ok(cwd) = authorize_existing_workspace(&state, &query.cwd) else {
        return failure(StatusCode::FORBIDDEN, "Access denied");
    };
    query.cwd = cwd.to_string_lossy().into_owned();
    skills::content(&state.agent_dir, query).await
}

async fn install_skill(State(state): State<AppState>, Json(mut input): Json<Value>) -> Response {
    if input.get("scope").and_then(Value::as_str) == Some("project")
        && let Err(response) = authorize_json_workspace(&state, &mut input)
    {
        return response;
    }
    skills::install(&state.agent_dir, input).await
}

async fn check_skills(State(state): State<AppState>, Json(mut input): Json<Value>) -> Response {
    if let Err(response) = authorize_json_workspace(&state, &mut input) {
        return response;
    }
    skills::check_updates(input).await
}

async fn search_skills(Json(input): Json<Value>) -> Response {
    skills::search(input).await
}

async fn update_skill(State(state): State<AppState>, Json(mut input): Json<Value>) -> Response {
    if let Err(response) = authorize_json_workspace(&state, &mut input) {
        return response;
    }
    skills::update(&state.agent_dir, input).await
}

async fn plugin_action(State(state): State<AppState>, Json(mut input): Json<Value>) -> Response {
    if let Err(response) = authorize_json_workspace(&state, &mut input) {
        return response;
    }
    plugins::action(&state, input).await
}

async fn plugin_toggle(State(state): State<AppState>, Json(mut input): Json<Value>) -> Response {
    if let Err(response) = authorize_json_workspace(&state, &mut input) {
        return response;
    }
    plugins::toggle(&state, input)
}

async fn plugin_inventory(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Response {
    let Some(cwd) = query.get("cwd") else { return failure(StatusCode::BAD_REQUEST, "cwd required") };
    let Ok(cwd) = authorize_existing_workspace(&state, cwd) else {
        return failure(StatusCode::FORBIDDEN, "Access denied");
    };
    plugin_inventory::list(&state.agent_dir, &cwd).await
}

async fn check_plugin_updates(State(state): State<AppState>, Json(mut input): Json<Value>) -> Response {
    if let Err(response) = authorize_json_workspace(&state, &mut input) {
        return response;
    }
    plugin_updates::check(&state.agent_dir, input).await
}

async fn provider_usage(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let Some(provider_id) = input.get("providerId").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"Unsupported provider"}))).into_response();
    };
    if !provider_usage::is_provider_id(provider_id) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"Unsupported provider"}))).into_response();
    }
    Json(provider_usage::query(&state.agent_dir, provider_id).await).into_response()
}

static CODEMODE_STATUS_CACHE: OnceLock<tokio::sync::Mutex<Option<(Option<Instant>, Value)>>> = OnceLock::new();

async fn codemode_status() -> Json<Value> {
    let cache = CODEMODE_STATUS_CACHE.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut cached = cache.lock().await;
    if let Some((expires_at, value)) = cached.as_ref()
        && expires_at.is_none_or(|expires_at| Instant::now() < expires_at)
    {
        return Json(value.clone());
    }
    let status = match pi_codemode::CodemodeSandbox::new(Default::default()) {
        Ok(sandbox) => match sandbox.execute("return 1 + 1;", Default::default()).await {
            Ok(result) if result.is_ok() => json!({"checked":true,"available":true}),
            Ok(result) => json!({
                "checked":true,
                "available":false,
                "reason":result.error().map(|error| error.message.as_str()).unwrap_or("sandbox self-test failed"),
            }),
            Err(error) => json!({"checked":true,"available":false,"reason":error.to_string()}),
        },
        Err(error) => json!({"checked":true,"available":false,"reason":error.to_string()}),
    };
    let available = status.get("available").and_then(Value::as_bool) == Some(true);
    *cached = Some(((!available).then(|| Instant::now() + Duration::from_secs(60)), status.clone()));
    Json(status)
}

async fn create_terminal(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let Some(cwd) = input.get("cwd").and_then(Value::as_str) else {
        return failure(StatusCode::BAD_REQUEST, "cwd required");
    };
    let Some(cwd) = path_access::allowed_existing_path(&state, Path::new(cwd)) else {
        if Path::new(cwd).is_dir() {
            return failure(StatusCode::FORBIDDEN, "Access denied");
        }
        return failure(StatusCode::BAD_REQUEST, "cwd must be a directory");
    };
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, "cwd must be a directory");
    }
    let cols = input.get("cols").and_then(Value::as_u64).unwrap_or(80).clamp(2, 1000) as u16;
    let rows = input.get("rows").and_then(Value::as_u64).unwrap_or(24).clamp(2, 1000) as u16;
    let requested_id = input.get("id").and_then(Value::as_str);
    match state.terminals.create(&cwd, cols, rows, requested_id) {
        Ok(id) => Json(json!({"id":id})).into_response(),
        Err(error) => failure(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn terminal_status(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    state.terminals.cwd(&id).map_or_else(
        || failure(StatusCode::NOT_FOUND, "Terminal expired or closed"),
        |cwd| Json(json!({"id":id,"cwd":cwd})).into_response(),
    )
}

async fn terminal_command(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Value>,
) -> Response {
    match input.get("type").and_then(Value::as_str) {
        Some("input") => {
            let Some(data) = input.get("data").and_then(Value::as_str).filter(|data| data.len() <= 64 * 1024) else {
                return failure(StatusCode::BAD_REQUEST, "Invalid terminal command");
            };
            if state.terminals.write(&id, data) {
                ok(json!({}))
            } else {
                failure(StatusCode::NOT_FOUND, "Terminal not found")
            }
        }
        Some("resize") => {
            let cols = input.get("cols").and_then(Value::as_u64).filter(|value| (2..=1000).contains(value));
            let rows = input.get("rows").and_then(Value::as_u64).filter(|value| (1..=1000).contains(value));
            let (Some(cols), Some(rows)) = (cols, rows) else {
                return failure(StatusCode::BAD_REQUEST, "Invalid terminal command");
            };
            if state.terminals.resize(&id, cols as u16, rows.max(2) as u16) {
                ok(json!({}))
            } else {
                failure(StatusCode::NOT_FOUND, "Terminal not found")
            }
        }
        _ => failure(StatusCode::BAD_REQUEST, "Invalid terminal command"),
    }
}

async fn delete_terminal(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    state.terminals.kill(&id);
    ok(json!({}))
}

async fn terminal_events(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .or_else(|| query.get("after").map(String::as_str))
        .and_then(|value| value.parse::<u64>().ok());
    let Some(subscription) = state.terminals.subscribe(&id, cursor) else {
        return failure(StatusCode::NOT_FOUND, "Terminal not found");
    };
    let holder = query.get("holder").map(String::as_str);
    let forwarded_for = headers.get("x-forwarded-for").and_then(|value| value.to_str().ok());
    let user_agent = headers.get("user-agent").and_then(|value| value.to_str().ok());
    let page_stream = terminal::register_page_stream(holder, forwarded_for, user_agent);
    let mut initial = vec![subscription.initial.clone()];
    if subscription.exited {
        initial.push(terminal::TerminalEvent::Exit { exit_code: subscription.exit_code.unwrap_or(0) });
    }
    let events = futures::stream::unfold(Some((subscription, initial.into_iter(), page_stream)), |state| async move {
        let (mut subscription, mut pending, page_stream) = state?;
        let event = match pending.next() {
            Some(event) => event,
            None => subscription.recv().await,
        };
        let finished = matches!(event, terminal::TerminalEvent::Exit { .. } | terminal::TerminalEvent::Closed);
        let id = match &event {
            terminal::TerminalEvent::Output { offset, .. } => Some(offset.to_string()),
            _ => None,
        };
        let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
        let mut wire = Event::default().data(data);
        if let Some(id) = id {
            wire = wire.id(id);
        }
        let next = (!finished).then_some((subscription, pending, page_stream));
        Some((Ok::<_, std::convert::Infallible>(wire), next))
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(30))).into_response()
}

fn is_suppressible_model_glob(pattern: &str) -> bool {
    if !pattern.contains(['*', '?', '[']) {
        return false;
    }
    pattern
        .rsplit_once(':')
        .map(|(_, suffix)| matches!(suffix, "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"))
        .unwrap_or(true)
}

async fn cors(request: Request, next: Next) -> Response {
    let origin = request.headers().get(header::ORIGIN).and_then(|value| value.to_str().ok());
    let allowed_origin = origin
        .filter(|origin| {
            [
                "http://localhost",
                "http://127.0.0.1",
                "http://[::1]",
                "https://localhost",
                "https://127.0.0.1",
                "https://[::1]",
            ]
            .iter()
            .any(|allowed| {
                *origin == *allowed
                    || origin.strip_prefix(allowed).is_some_and(|suffix| {
                        suffix.starts_with(':') && suffix[1..].bytes().all(|byte| byte.is_ascii_digit())
                    })
            })
        })
        .map(str::to_string);
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        if let Some(origin) = allowed_origin {
            if let Ok(origin) = HeaderValue::from_str(&origin) {
                headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
            }
            headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        }
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"));
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("content-type"));
        return response;
    }
    let mut response = next.run(request).await;
    if let Some(origin) = allowed_origin {
        if let Ok(origin) = HeaderValue::from_str(&origin) {
            response.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        }
        response.headers_mut().insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

fn session_catalog_json(info: &SessionInfo, path_to_id: &HashMap<String, String>, project_fields: &Value) -> Value {
    let mut value = json!({
        "id": info.id,
        "cwd": info.cwd,
        "path": info.path,
        "created": pi_coding_agent::session::format_iso(info.created),
        "modified": pi_coding_agent::session::format_iso(info.modified),
        "messageCount": info.message_count,
        "firstMessage": catalog_first_message(&info.first_message),
        "transient": false,
    });
    if let Some(name) = &info.name {
        value["name"] = json!(name);
    }
    let (mut relation, relation_parent, _is_side) = session_catalog_relation(info);
    let parent_id =
        relation_parent.or_else(|| info.parent_session_path.as_ref().and_then(|path| path_to_id.get(path).cloned()));
    if let Some(parent_id) = &parent_id {
        value["parentSessionId"] = json!(parent_id);
    }
    if relation.as_ref().is_some_and(|item| item["kind"] == "fork") {
        if let Some(parent_id) = &parent_id {
            relation.as_mut().unwrap()["originSessionId"] = json!(parent_id);
        }
    }
    if let Some(relation) = relation {
        value["relation"] = relation;
    }
    if let Some(project) = project_fields.as_object() {
        for (key, field) in project {
            value[key] = field.clone();
        }
    }
    value
}

fn catalog_first_message(text: &str) -> String {
    let display = skill_expansion_to_command(text).unwrap_or_else(|| text.to_string());
    let mut units = 0;
    display
        .chars()
        .take_while(|ch| {
            let width = ch.len_utf16();
            if units + width > 500 {
                false
            } else {
                units += width;
                true
            }
        })
        .collect()
}

fn skill_expansion_to_command(text: &str) -> Option<String> {
    let rest = text.strip_prefix("<skill name=\"")?;
    let (name, rest) = rest.split_once("\" location=\"")?;
    let (_, rest) = rest.split_once("\">\n")?;
    let (reference, body) = rest.split_once("\n\n")?;
    if !reference.starts_with("References are relative to ") || !reference.ends_with('.') {
        return None;
    }
    let close = body.rfind("\n</skill>")?;
    let args = body[close + "\n</skill>".len()..].strip_prefix("\n\n");
    if !body[close + "\n</skill>".len()..].is_empty() && args.is_none() {
        return None;
    }
    Some(match args {
        Some(args) => format!("/skill:{name} {args}"),
        None => format!("/skill:{name}"),
    })
}

fn session_catalog_relation(info: &SessionInfo) -> (Option<Value>, Option<String>, bool) {
    use std::io::{BufRead, BufReader, Read};
    let Ok(file) = fs::File::open(&info.path) else { return (None, None, false) };
    let reader = BufReader::new(file.take(256 * 1024));
    let (mut relation, mut parent, mut is_side) = (None, None, false);
    for line in reader.lines().take(5_000).flatten().skip(1) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
        let kind = entry["customType"].as_str().unwrap_or_default();
        let data = &entry["data"];
        if kind == "pi-web:side" {
            is_side = true;
            parent = data["parentSessionId"].as_str().map(str::to_string);
            relation = parent.as_ref().map(|id| json!({"kind":"side","parentSessionId":id}));
            continue;
        }
    }
    if relation.is_none() && info.parent_session_path.is_some() {
        relation = Some(json!({"kind":"fork"}));
    }
    (relation, parent, is_side)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let backend = std::env::current_exe()?;
    let directory =
        backend.parent().ok_or_else(|| anyhow::anyhow!("Cannot locate the pi-web-backend executable directory"))?;
    let pi = directory.join(format!("pi{}", std::env::consts::EXE_SUFFIX));
    if !pi.is_file() {
        anyhow::bail!("Required Pi executable was not found next to pi-web-backend: {}", pi.display());
    }

    let pi_path = pi.to_string_lossy().into_owned();
    unsafe { std::env::set_var("PI_BIN", &pi_path) };
    let configured_pi = std::env::var("PI_BIN")
        .map_err(|error| anyhow::anyhow!("Failed to read PI_BIN after configuring it: {error}"))?;
    if configured_pi != pi_path {
        anyhow::bail!("PI_BIN did not retain the configured Pi executable path: {configured_pi}");
    }

    let agent_dir = agent_dir();
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.cancel();
    });
    let state = AppState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        opening_sessions: Arc::new(Mutex::new(HashMap::new())),
        login_callbacks: Arc::new(Mutex::new(HashMap::new())),
        agent_dir,
        session_catalog_version: Arc::new(Mutex::new((None, 0))),
        started_at: pi_ai::now_ms(),
        stop: stop.clone(),
        terminals: terminal::TerminalManager::default(),
        auth_providers_cache: Arc::new(Mutex::new(None)),
    };
    let app = http::router(state.clone());
    let address: SocketAddr = std::env::var("PI_WEB_BACKEND_ADDR").unwrap_or_else(|_| "0.0.0.0:3030".into()).parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tokio::spawn(sessions::reap_idle_sessions(state.clone()));
    println!("pi-web-backend listening on http://{address}");
    if let Some(static_dir) = static_files::static_dir() {
        println!("pi-web-backend serving static web files from {}", static_dir.display());
    }
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await?;
    state.terminals.shutdown();
    let sessions = lock(&state.sessions).values().cloned().collect::<Vec<_>>();
    for session in sessions {
        session.dispose().await;
    }
    Ok(())
}
