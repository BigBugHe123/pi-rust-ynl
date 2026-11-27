//! Browser-facing `/api/*` aliases for deployments that serve the UI statically.
//!
//! These routes intentionally reuse Rust handlers directly. Endpoints whose Next
//! adapters aggregate data or reshape requests remain outside this router.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as RoutePath, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde_json::{Map, Value, json};

use super::*;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/default-cwd", post(default_cwd::create))
        .route("/api/terminals", post(create_terminal))
        .route("/api/terminal/{id}", get(terminal_status).post(terminal_command).delete(delete_terminal))
        .route("/api/terminal/{id}/events", get(terminal_events))
        .route("/api/models", get(models))
        .route("/api/models-config", get(models::config::get).put(models_config_put))
        .route("/api/models-config/catalog", get(models::catalog::get))
        .route("/api/models-config/test", post(test_model))
        .route("/api/models-config/discover", post(discover_models))
        .route("/api/models/default", axum::routing::put(save_default_preferences))
        .route("/api/models/enabled", get(models::enabled::get).put(models::enabled::put))
        .route("/api/models/refresh", post(refresh_models))
        .route("/api/tools/settings", get(tool_settings::get).put(tool_settings::put))
        .route("/api/cache-miss-notices", get(tool_settings::cache_get).put(tool_settings::cache_put))
        .route("/api/prompts", get(prompts::get).put(prompts::put).delete(prompts::delete))
        .route("/api/subagents/settings", get(subagent_settings::get).put(subagent_settings::put))
        .route(
            "/api/subagents/profiles",
            get(subagent_profiles::list)
                .put(subagent_profiles::save)
                .patch(subagent_profiles::patch)
                .delete(subagent_profiles::delete),
        )
        .route("/api/project-trust", get(project_trust::get).post(project_trust::post))
        .route("/api/project-trust/fresh-check", post(project_trust::fresh_check))
        .route("/api/mcp/test", post(mcp::test::test_mcp_server))
        .route("/api/mcp", get(mcp::config_read::overview).post(mcp::actions::action))
        .route("/api/mcp/sign-in", post(mcp::test::start_mcp_sign_in))
        .route(
            "/api/mcp/sign-in/{id}",
            get(mcp::test::get_mcp_sign_in).post(mcp::test::redirect_mcp_sign_in).delete(mcp::test::cancel_mcp_sign_in),
        )
        .route("/api/mcp/sign-in/{id}/redirect", post(mcp::test::redirect_mcp_sign_in))
        .route("/api/skills", get(list_skills).patch(toggle_skills))
        .route("/api/skills/content", get(skill_content))
        .route("/api/skills/install", post(install_skill))
        .route("/api/skills/check", post(check_skills_compat))
        .route("/api/skills/search", post(search_skills))
        .route("/api/skills/update", post(update_skill_compat))
        .route("/api/web-auth", get(web_auth::status).post(web_auth::login).delete(web_auth::logout))
        .route("/api/plugins", get(plugin_inventory).post(plugin_action_compat))
        .route("/api/plugins/check", post(check_plugin_updates))
        .route("/api/provider-usage/query", post(provider_usage))
        .route("/api/home", get(cwd::home))
        .route("/api/app-update", get(app_update::get))
        .route("/api/cwd/validate", post(cwd::validate))
        .route("/api/cwd/browse", get(cwd::browse_get).post(cwd::browse_post))
        .route("/api/cwd/allowed-roots", get(cwd::allowed_roots))
        .route("/api/git/status", get(git_changes::status))
        .route("/api/git/diff", get(git_changes::diff))
        .route("/api/file-index", get(file_index::get))
        .route(
            "/api/files/{*path}",
            get(workspace_files::get)
                .post(workspace_files::post)
                .layer(DefaultBodyLimit::max(workspace_files::MAX_UPLOAD_REQUEST_BYTES)),
        )
        .route("/api/push/config", get(push::config))
        .route("/api/push/subscribe", post(push::subscribe))
        .route("/api/worktrees", get(worktrees::get).post(worktrees::post).delete(worktrees::delete))
        .route("/api/open-in-explorer", get(open_in_explorer::get).post(open_in_explorer::post))
        .route("/api/auth/providers", get(provider_auth::auth_providers))
        .route("/api/auth/api-key/{provider}", post(api_key_auth_compat).delete(delete_api_key_compat))
        .route("/api/auth/logout/{provider}", post(delete_oauth_compat))
        .route("/api/auth/login/{provider}", get(oauth_login_compat).post(provider_auth::oauth_login_callback))
        .route("/api/sessions", get(list_sessions).post(sessions::create_session))
        .route("/api/sessions/running", get(running_sessions))
        .route("/api/sessions/ui-state", get(session_ui_state::get).post(session_ui_state::post))
        .route("/api/sessions/delete", post(delete_sessions_compat))
        .route("/api/sessions/stats", post(session_stats_compat))
        .route("/api/sessions/search", get(sessions::search_sessions))
        .route("/api/agent/new", post(create_session_compat))
        .route("/api/agent/running", get(running_sessions))
        .route("/api/agent/debug", get(sessions::debug_snapshot))
        .route("/api/agent/{id}", get(sessions::status).post(agent_command_compat))
        .route("/api/agent/{id}/lease", post(sessions::lease))
        .route("/api/agent/{id}/events", get(sessions::events))
        .route("/api/agent/{id}/bash-output", get(bash_output::get))
        .route("/api/sessions/{id}/context", get(sessions::session_context))
        .route("/api/sessions/{id}/export", get(sessions::export_session_html))
        .route("/api/sessions/{id}/state", get(sessions::session_state_route))
        .route("/api/sessions/{id}/auto-name", post(sessions::auto_name_session))
        .route("/api/sessions/{id}/fork", post(fork_session_compat))
        .route("/api/sessions/{id}/entries/{entry_id}/thinking", get(sessions::session_entry_thinking))
        .route(
            "/api/sessions/{id}/entries/{entry_id}/tool-result-image",
            get(sessions::session_entry_tool_result_image),
        )
        .route(
            "/api/sessions/{id}",
            get(sessions::session_detail_snapshot).patch(rename_session_compat).delete(delete_session_compat),
        )
}

async fn create_session_compat(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let command_type = input.get("type").and_then(Value::as_str).map(str::to_owned);
    let cwd = input.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty()).map(str::to_owned);
    let Some(cwd) = cwd else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cwd is required"}))).into_response();
    };
    if let Some(level) = input.get("thinkingLevel") {
        let valid = matches!(level.as_str(), Some("off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"));
        if !valid {
            let mut response = (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error":format!("Invalid thinking level: {}", js_value_string(level))})),
            )
                .into_response();
            if command_type.as_deref() == Some("prompt") {
                response = decorate_agent_error(response, true, Some(&cwd), !Path::new(&cwd).is_dir()).await;
            }
            return response;
        }
    }
    let request = match serde_json::from_value::<sessions::CreateSessionRequest>(input) {
        Ok(request) => request,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":error.to_string()}))).into_response();
        }
    };
    let response = sessions::create_session(State(state), Json(request)).await;
    decorate_agent_error(response, command_type.as_deref() == Some("prompt"), Some(&cwd), !Path::new(&cwd).is_dir())
        .await
}

fn js_value_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

async fn agent_command_compat(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Json(command): Json<Value>,
) -> Response {
    let command_type = command.get("type").and_then(Value::as_str).map(str::to_owned);
    let cwd = list_all_sessions(None, &state.agent_dir)
        .into_iter()
        .find(|session| session.id == id)
        .map(|session| session.cwd);
    let cwd_missing = cwd.as_deref().is_some_and(|cwd| !Path::new(cwd).is_dir());
    let response = sessions::command(State(state), RoutePath(id), Json(command)).await;
    decorate_agent_error(response, command_type.as_deref() == Some("prompt"), cwd.as_deref(), cwd_missing).await
}

async fn decorate_agent_error(
    response: Response,
    is_prompt: bool,
    cwd_hint: Option<&str>,
    cwd_missing: bool,
) -> Response {
    if response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let Some(object) = value.as_object_mut() else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let Some(error) = object.get("error").and_then(Value::as_str).map(str::to_owned) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    let missing_cwd = if object.get("reason").and_then(Value::as_str) == Some("cwd-missing") {
        object.get("cwd").and_then(Value::as_str).map(str::to_owned).or_else(|| cwd_hint.map(str::to_owned))
    } else if cwd_missing || error.to_ascii_lowercase().contains("does not exist") {
        cwd_hint.map(str::to_owned).or_else(|| {
            error.split_once("does not exist:").map(|(_, path)| path.trim().to_owned()).filter(|path| !path.is_empty())
        })
    } else {
        None
    };
    if let Some(cwd) = missing_cwd {
        object.insert("error".to_owned(), json!(format!("Directory does not exist: {cwd}")));
        object.insert("reason".to_owned(), json!("cwd-missing"));
        object.insert("cwd".to_owned(), json!(cwd));
    }
    let extension_reported = object.get("code").and_then(Value::as_str) == Some("extension_reported");
    if is_prompt {
        object.insert(
            "code".to_owned(),
            json!(if extension_reported { "extension_reported" } else { "prompt_rejected" }),
        );
        object.insert("accepted".to_owned(), json!(false));
    }
    parts.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Response::from_parts(parts, Body::from(serde_json::to_vec(&value).unwrap_or(bytes.to_vec())))
}

async fn delete_api_key_compat(State(state): State<AppState>, RoutePath(provider): RoutePath<String>) -> Response {
    let response =
        provider_auth::delete_provider_credential(State(state), RoutePath((provider, "api-key".to_owned()))).await;
    if response.status().is_success() {
        models::cache::invalidate();
    }
    response
}

async fn delete_oauth_compat(State(state): State<AppState>, RoutePath(provider): RoutePath<String>) -> Response {
    let response =
        provider_auth::delete_provider_credential(State(state), RoutePath((provider, "oauth".to_owned()))).await;
    if response.status().is_success() {
        models::cache::invalidate();
    }
    response
}

async fn api_key_auth_compat(
    State(state): State<AppState>,
    RoutePath(provider): RoutePath<String>,
    request: Request,
) -> Response {
    let response = provider_auth::api_key_auth(State(state), RoutePath(provider), request).await;
    if response.status().is_success() {
        models::cache::invalidate();
    }
    response
}

async fn models_config_put(State(state): State<AppState>, Json(value): Json<Value>) -> Response {
    let value = match validate_models_config(value) {
        Ok(value) => value,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    };
    let response = models::config::put(State(state), Json(value)).await;
    if response.status().is_success() {
        models::cache::invalidate();
    }
    response
}

fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

fn positive_context_window(value: &Value) -> Option<Value> {
    const MAX_SAFE: u64 = 9_007_199_254_740_991;
    match value {
        Value::String(value) => {
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let parsed = value.parse::<u64>().ok()?;
            (parsed > 0 && parsed <= MAX_SAFE).then_some(Value::from(parsed))
        }
        Value::Number(value) => {
            let number = value.as_f64()?;
            (number.is_finite() && number > 0.0).then(|| Value::Number(value.clone()))
        }
        _ => None,
    }
}

fn positive_image_byte_count(value: &Value) -> Option<Value> {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    let number = match value {
        Value::String(value) => {
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            value.parse::<f64>().ok()?
        }
        Value::Number(value) => value.as_f64()?,
        _ => return None,
    };
    if !number.is_finite() || number < 1.0 || number.fract() != 0.0 || number > MAX_SAFE {
        return None;
    }
    Some(Value::from(number as u64))
}

fn validate_models_config(mut config: Value) -> Result<Value, String> {
    let Some(providers) = config.get_mut("providers").and_then(Value::as_object_mut) else { return Ok(config) };

    for (provider_id, provider) in providers.iter_mut() {
        let Some(provider) = provider.as_object_mut() else { continue };
        let Some(overrides) = provider.get_mut("modelOverrides").and_then(Value::as_object_mut) else { continue };
        for (model_id, override_value) in overrides.iter_mut() {
            let Some(override_value) = override_value.as_object_mut() else { continue };
            let Some(context_window) = override_value.get("contextWindow") else { continue };
            let Some(normalized) = positive_context_window(context_window) else {
                return Err(format!("Context window of {provider_id}/{model_id} must be a positive number of tokens"));
            };
            override_value.insert("contextWindow".to_owned(), normalized);
        }
    }

    for (provider_id, provider) in providers.iter() {
        let Some(provider) = provider.as_object() else { continue };
        if js_truthy(provider.get("oauth")) && !js_truthy(provider.get("baseUrl")) {
            return Err(format!("Provider {provider_id} needs a Base URL"));
        }
        let has_models = provider.get("models").and_then(Value::as_array).is_some_and(|models| {
            models.iter().any(|model| {
                let Some(model) = model.as_object() else { return true };
                !model.get("id").and_then(Value::as_str).is_some_and(|id| id.trim().is_empty())
            })
        });
        let has_overrides =
            provider.get("modelOverrides").and_then(Value::as_object).is_some_and(|overrides| !overrides.is_empty());
        if !has_models
            && !js_truthy(provider.get("baseUrl"))
            && !js_truthy(provider.get("headers"))
            && !js_truthy(provider.get("compat"))
            && !has_overrides
            && !js_truthy(provider.get("apiKey"))
            && !js_truthy(provider.get("oauth"))
            && !provider.contains_key("authHeader")
        {
            return Err(format!("Provider {provider_id} needs a Base URL or at least one model"));
        }
    }

    for (provider_id, provider) in providers.iter() {
        let Some(models) = provider.get("models").and_then(Value::as_array) else { continue };
        for model in models {
            let Some(model) = model.as_object() else { continue };
            if model.get("id").and_then(Value::as_str).is_some_and(|id| id.trim().is_empty()) {
                continue;
            }
            let max_bytes = model
                .get("inputLimits")
                .and_then(Value::as_object)
                .and_then(|limits| limits.get("images"))
                .and_then(Value::as_object)
                .and_then(|images| images.get("resize"))
                .and_then(Value::as_object)
                .and_then(|resize| resize.get("maxBytes"));
            let Some(value) = max_bytes else { continue };
            if positive_image_byte_count(value).is_none() {
                let model_id = model.get("id").and_then(Value::as_str).unwrap_or("?");
                return Err(format!(
                    "Image size limit of {provider_id}/{model_id} must be a positive whole number of bytes"
                ));
            }
        }
    }
    Ok(config)
}

async fn oauth_login_compat(State(state): State<AppState>, RoutePath(provider): RoutePath<String>) -> Response {
    use std::convert::Infallible;

    let response = provider_auth::oauth_login(State(state), RoutePath(provider)).await;
    let (parts, body) = response.into_parts();
    let mut upstream = body.into_data_stream();
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(8);
    tokio::spawn(async move {
        let mut pending = Vec::<u8>::new();
        while let Some(Ok(chunk)) = upstream.next().await {
            pending.extend_from_slice(&chunk);
            while let Some(end) = pending.windows(2).position(|window| window == b"\n\n") {
                let frame = pending.drain(..end + 2).collect::<Vec<_>>();
                if frame.windows(b"\"type\":\"success\"".len()).any(|window| window == b"\"type\":\"success\"")
                    || frame.windows(b"\"type\": \"success\"".len()).any(|window| window == b"\"type\": \"success\"")
                {
                    models::cache::invalidate();
                }
                if sender.send(Bytes::from(frame)).await.is_err() {
                    return;
                }
            }
        }
        if !pending.is_empty() {
            let _ = sender.send(Bytes::from(pending)).await;
        }
    });
    let body = futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (Ok::<Bytes, Infallible>(chunk), receiver))
    });
    Response::from_parts(parts, Body::from_stream(body))
}

async fn unwrap_data(response: Response) -> Response {
    if !response.status().is_success() {
        return response;
    }
    match parse_json_response(response).await {
        Ok(value) => Json(value.get("data").cloned().unwrap_or(value)).into_response(),
        Err(response) => response,
    }
}

async fn delete_sessions_compat(
    State(state): State<AppState>,
    Json(input): Json<sessions::DeleteSessionsRequest>,
) -> Response {
    unwrap_data(sessions::delete_sessions(State(state), Json(input)).await).await
}

async fn session_stats_compat(
    State(state): State<AppState>,
    Json(input): Json<sessions::SessionStatsRequest>,
) -> Response {
    let response = sessions::session_stats(State(state), Json(input)).await;
    match parse_json_response(response).await {
        Ok(value) => {
            let mut response =
                Json(json!({"stats":value.pointer("/data/stats").cloned().unwrap_or(Value::Null)})).into_response();
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(response) => response,
    }
}

async fn rename_session_compat(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Value>,
) -> Response {
    let response = sessions::rename_session(State(state), RoutePath(id), Json(input)).await;
    match parse_json_response(response).await {
        Ok(value) if value.pointer("/data/ok").and_then(Value::as_bool) == Some(true) => {
            Json(json!({"ok":true})).into_response()
        }
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

async fn delete_session_compat(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    let response = sessions::delete_session(State(state), RoutePath(id)).await;
    match parse_json_response(response).await {
        Ok(value) if value.pointer("/data/deleted").is_some() => Json(json!({"ok":true})).into_response(),
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

async fn fork_session_compat(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<sessions::ForkSessionRequest>,
) -> Response {
    let response = sessions::fork_session_file(State(state), RoutePath(id), Json(input)).await;
    let mut response = unwrap_data(response).await;
    if response.status().is_success() {
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

async fn plugin_action_compat(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let action = body.get("action").and_then(Value::as_str).unwrap_or_default();
    if !matches!(action, "install" | "remove" | "update" | "enable" | "disable") {
        let error =
            if action.is_empty() { "action required".to_owned() } else { format!("Unsupported action: {action}") };
        return (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response();
    }
    let cwd = body.get("cwd").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mutation = if matches!(action, "enable" | "disable") {
        plugin_toggle(State(state.clone()), Json(body.clone())).await
    } else {
        plugin_action(State(state.clone()), Json(body.clone())).await
    };
    let results = match parse_json_response(mutation).await {
        Ok(value) => value.get("results").cloned(),
        Err(response) => return response,
    };
    let inventory = match parse_json_response(
        plugin_inventory(State(state), Query(HashMap::from([("cwd".to_owned(), cwd)]))).await,
    )
    .await
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    if body.get("packages").is_some() {
        let mut inventory = inventory;
        if let Some(object) = inventory.as_object_mut() {
            object.insert("results".to_owned(), results.unwrap_or_else(|| json!([])));
        }
        Json(inventory).into_response()
    } else {
        Json(inventory).into_response()
    }
}

async fn check_skills_compat(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let cwd = body.get("cwd").and_then(Value::as_str).unwrap_or_default().to_owned();
    if cwd.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cwd required"}))).into_response();
    }
    let package = body.get("package").and_then(Value::as_str);
    let scope = body.get("scope").and_then(Value::as_str);
    if (package.is_some() && !matches!(scope, Some("global" | "project"))) || (package.is_none() && scope.is_some()) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"package and scope must be provided together"})))
            .into_response();
    }
    let listed = match parse_json_response(
        list_skills(State(state.clone()), Query(HashMap::from([("cwd".to_owned(), cwd.clone())]))).await,
    )
    .await
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    let installs = listed
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|skill| skill.get("install"))
        .filter(|install| {
            package.is_none_or(|package| {
                install.get("package").and_then(Value::as_str) == Some(package)
                    && install.get("scope").and_then(Value::as_str) == scope
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    if package.is_some() && installs.is_empty() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"Installed skill not found"}))).into_response();
    }
    check_skills(State(state), Json(json!({"cwd":cwd,"installs":installs}))).await
}

async fn update_skill_compat(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let cwd = body.get("cwd").and_then(Value::as_str).unwrap_or_default().to_owned();
    let package = body.get("package").and_then(Value::as_str).unwrap_or_default();
    let scope = body.get("scope").and_then(Value::as_str);
    if cwd.is_empty() || package.is_empty() || !matches!(scope, Some("global" | "project")) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cwd, package, and scope are required"})))
            .into_response();
    }
    let listed = match parse_json_response(
        list_skills(State(state.clone()), Query(HashMap::from([("cwd".to_owned(), cwd.clone())]))).await,
    )
    .await
    {
        Ok(value) => value,
        Err(response) => return response,
    };
    let skill = listed
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|skill| {
            skill.get("install").is_some_and(|install| {
                install.get("package").and_then(Value::as_str) == Some(package)
                    && install.get("scope").and_then(Value::as_str) == scope
            })
        })
        .cloned();
    let Some(install) = skill.as_ref().and_then(|skill| skill.get("install")).cloned() else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"Installed skill not found"}))).into_response();
    };
    if install.get("canCheckForUpdates").and_then(Value::as_bool) != Some(true) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"This skill cannot be updated automatically"})))
            .into_response();
    }
    let update = update_skill(State(state.clone()), Json(json!({"cwd":cwd,"install":install}))).await;
    let mut result = match parse_json_response(update).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let refreshed =
        match parse_json_response(list_skills(State(state), Query(HashMap::from([("cwd".to_owned(), cwd)]))).await)
            .await
        {
            Ok(value) => value,
            Err(_) => Value::Null,
        };
    let updated = refreshed
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|skill| {
            skill.get("install").is_some_and(|install| {
                install.get("package").and_then(Value::as_str) == Some(package)
                    && install.get("scope").and_then(Value::as_str) == scope
            })
        })
        .cloned();
    if let Some(object) = result.as_object_mut() {
        if let Some(updated) = updated {
            object.insert("skill".to_owned(), updated);
        }
    }
    Json(result).into_response()
}

async fn parse_json_response(response: Response) -> Result<Value, Response> {
    if !response.status().is_success() {
        return Err(response);
    }
    let bytes = match axum::body::to_bytes(response.into_body(), 128 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(
                (StatusCode::BAD_GATEWAY, Json(json!({"error":"Could not read Rust API response"}))).into_response()
            );
        }
    };
    serde_json::from_slice(&bytes)
        .map_err(|_| (StatusCode::BAD_GATEWAY, Json(json!({"error":"Rust API returned invalid JSON"}))).into_response())
}

async fn list_sessions(State(state): State<AppState>) -> Response {
    let catalog = match parse_json_response(sessions::list_sessions(State(state.clone())).await).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let running = match parse_json_response(sessions::running_sessions(State(state)).await).await {
        Ok(value) => value,
        Err(_) => Value::Null,
    };

    let mut body = Map::new();
    body.insert("sessions".into(), catalog.get("sessions").cloned().unwrap_or_else(|| json!([])));
    body.insert("sessionListVersion".into(), catalog.get("sessionListVersion").cloned().unwrap_or_else(|| json!(0)));
    body.insert("runningSessionIds".into(), running.get("runningSessionIds").cloned().unwrap_or_else(|| json!([])));
    body.insert(
        "awaitingInputSessionIds".into(),
        running.get("awaitingInputSessionIds").cloned().unwrap_or_else(|| json!([])),
    );
    body.insert("completionNotificationSuppressedSessionIds".into(), json!([]));
    let mut response = Json(Value::Object(body)).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn running_sessions(State(state): State<AppState>) -> Response {
    let running = match parse_json_response(sessions::running_sessions(State(state)).await).await {
        Ok(value) => value,
        Err(_) => Value::Null,
    };
    let mut body = running.as_object().cloned().unwrap_or_default();
    body.entry("sessionListVersion").or_insert_with(|| json!(0));
    body.entry("runningSessionIds").or_insert_with(|| json!([]));
    body.entry("awaitingInputSessionIds").or_insert_with(|| json!([]));
    body.insert("completionNotificationSuppressedSessionIds".into(), json!([]));
    body.insert("subagentsActiveSessionIds".into(), json!([]));
    body.entry("sessionUiStateRevision").or_insert(Value::Null);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default();
    body.insert("serverNow".into(), json!(now_ms));
    let mut response = Json(Value::Object(body)).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
