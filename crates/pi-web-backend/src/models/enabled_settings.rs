use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::resources::paths::home_dir;
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};
use serde_json::{Value, json};

use crate::{AppState, failure, project_trust::resources_trusted};

fn settings_path(path: PathBuf) -> String {
    let home = home_dir();
    path.strip_prefix(&home).map_or_else(
        |_| path.display().to_string(),
        |relative| format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display()),
    )
}

fn open_settings(state: &AppState, cwd: &Path) -> Result<SettingsManager, String> {
    let trusted = resources_trusted(cwd, &state.agent_dir)?;
    Ok(SettingsManager::create(cwd, &state.agent_dir, SettingsOptions { project_trusted: trusted }))
}

fn response(settings: &SettingsManager, cwd: &Path, agent_dir: &Path) -> Value {
    let scope = if settings.project_value("enabledModels").is_some() { "project" } else { "global" };
    let raw = settings.raw_value(
        "enabledModels",
        if scope == "project" {
            pi_coding_agent::settings::SettingsScope::Project
        } else {
            pi_coding_agent::settings::SettingsScope::Global
        },
    );
    let patterns = raw.and_then(|value| value.as_array().cloned()).filter(|values| values.iter().all(Value::is_string));
    let path = if scope == "project" { cwd.join(".pi").join("settings.json") } else { agent_dir.join("settings.json") };
    json!({
        "patterns":patterns,
        "scope":scope,
        "settingsPath":settings_path(path),
        "editable":scope == "global",
    })
}

pub async fn get(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(cwd) = query.get("cwd").filter(|cwd| !cwd.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "cwd required");
    };
    let cwd = PathBuf::from(cwd);
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", cwd.display()));
    }
    let settings = match open_settings(&state, &cwd) {
        Ok(settings) => settings,
        Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    Json(response(&settings, &cwd, &state.agent_dir)).into_response()
}

pub fn put(state: &AppState, input: Value) -> Response {
    let Some(cwd) = input.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "cwd required");
    };
    let cwd = PathBuf::from(cwd);
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", cwd.display()));
    }
    let patterns = match input.get("patterns") {
        Some(Value::Null) | None => None,
        Some(Value::Array(patterns)) if patterns.iter().all(Value::is_string) => Some(patterns.clone()),
        _ => return failure(StatusCode::BAD_REQUEST, "patterns must be an array of strings or null"),
    };
    let settings = match open_settings(state, &cwd) {
        Ok(settings) => settings,
        Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    if settings.project_value("enabledModels").is_some() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Project settings override enabledModels","reason":"project-scope"})),
        )
            .into_response();
    }
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    settings.set_global_value("enabledModels", patterns.map(Value::Array));
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    Json(response(&settings, &cwd, &state.agent_dir)).into_response()
}
