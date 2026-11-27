use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::agent_session::{ModelRuntime, ModelRuntimeOptions};
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};
use serde_json::{Value, json};

use crate::{AppState, failure, path_access, project_trust::resources_trusted};

pub async fn save(state: &AppState, input: Value) -> Response {
    let requested_cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let Some(cwd) = path_access::allowed_existing_path(state, &requested_cwd) else {
        return failure(StatusCode::FORBIDDEN, "Access denied");
    };
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", cwd.display()));
    }
    let has_model = input.get("provider").is_some() || input.get("modelId").is_some();
    let model = if has_model {
        let (Some(provider), Some(model_id)) = (
            input.get("provider").and_then(Value::as_str).filter(|value| !value.is_empty()),
            input.get("modelId").and_then(Value::as_str).filter(|value| !value.is_empty()),
        ) else {
            return failure(StatusCode::BAD_REQUEST, "Expected provider and modelId, or a valid thinkingLevel");
        };
        Some((provider.to_owned(), model_id.to_owned()))
    } else {
        None
    };
    let thinking = if let Some(value) = input.get("thinkingLevel") {
        match serde_json::from_value::<pi_ai::ThinkingLevel>(value.clone()) {
            Ok(level) => Some(level),
            Err(_) => {
                return failure(StatusCode::BAD_REQUEST, "Expected provider and modelId, or a valid thinkingLevel");
            }
        }
    } else {
        None
    };
    if model.is_none() && thinking.is_none() {
        return failure(StatusCode::BAD_REQUEST, "Expected provider and modelId, or a valid thinkingLevel");
    }

    let model_runtime = if model.is_some() {
        Some(ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&state.agent_dir)).await)
    } else {
        None
    };
    let project_trusted = match resources_trusted(&cwd, &state.agent_dir) {
        Ok(trusted) => trusted,
        Err(error) => {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read project trust: {error}"));
        }
    };
    let settings = Arc::new(SettingsManager::create(&cwd, &state.agent_dir, SettingsOptions { project_trusted }));
    if let Some((provider, model_id)) = &model {
        if !model_runtime
            .as_ref()
            .expect("model validation creates runtime")
            .available_models()
            .iter()
            .any(|candidate| candidate.provider == *provider && candidate.id == *model_id)
        {
            return failure(StatusCode::NOT_FOUND, format!("Model not available: {provider}/{model_id}"));
        }
    }
    let project = settings.project_settings();
    let mut shadowed = Vec::new();
    if model.is_some() {
        if project.default_provider.is_some() {
            shadowed.push("defaultProvider");
        }
        if project.default_model.is_some() {
            shadowed.push("defaultModel");
        }
    }
    if thinking.is_some() && project.default_thinking_level.is_some() {
        shadowed.push("defaultThinkingLevel");
    }
    if !shadowed.is_empty() {
        let path = cwd.join(".pi").join("settings.json");
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":format!("{} sets {} for this project, so a global default would not apply here.", path.display(), shadowed.join(", ")),"reason":"project-scope","settingsPath":path,"keys":shadowed})),
        ).into_response();
    }
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    if let Some((provider, model_id)) = &model {
        settings.set_default_model_and_provider(provider, model_id);
    }
    if let Some(level) = thinking {
        settings.set_default_thinking_level(level);
    }
    settings.flush();
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    let mut result = json!({"ok":true});
    if let Some((provider, model_id)) = model {
        result["defaultModel"] = json!({"provider":provider,"modelId":model_id});
    }
    if let Some(level) = thinking {
        result["defaultThinkingLevel"] = json!(level.as_str());
    }
    Json(result).into_response()
}
