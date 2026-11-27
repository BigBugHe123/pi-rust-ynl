use std::path::Path;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};
use serde_json::{Value, json};

use crate::{AppState, failure};

const DEFAULT_TOOLS: &[&str] = &["read", "bash", "edit", "write"];
const BUILTIN_TOOLS: &[&str] = &["read", "grep", "find", "ls", "edit", "write", "bash"];

fn manager(state: &AppState) -> SettingsManager {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    SettingsManager::create(&cwd, &state.agent_dir, SettingsOptions::default())
}

fn raw_entries(settings: &SettingsManager) -> Result<Option<Vec<String>>, String> {
    match settings.global_value("defaultTools") {
        None => Ok(None),
        Some(Value::Array(values)) if values.iter().all(Value::is_string) => {
            Ok(Some(values.into_iter().filter_map(|value| value.as_str().map(str::to_owned)).collect()))
        }
        Some(_) => Err("Invalid settings.json: defaultTools must be an array of strings".into()),
    }
}

fn is_modifier(value: &str) -> bool {
    value.starts_with('+') || value.starts_with('-')
}

fn resolved(entries: &[String]) -> Vec<String> {
    let plain: Vec<String> = entries.iter().filter(|entry| !is_modifier(entry)).cloned().collect();
    let mut tools = if !plain.is_empty() || entries.is_empty() {
        plain
    } else {
        DEFAULT_TOOLS.iter().map(|name| (*name).to_owned()).collect()
    };
    for entry in entries.iter().filter(|entry| is_modifier(entry)) {
        let name = &entry[1..];
        if entry.starts_with('+') && !name.is_empty() && !tools.iter().any(|tool| tool == name) {
            tools.push(name.to_owned());
        } else if entry.starts_with('-') {
            if let Some(index) = tools.iter().position(|tool| tool == name) {
                tools.remove(index);
            }
        }
    }
    tools
}

fn with_builtin_tools(entries: Option<Vec<String>>, selected: &[String]) -> Option<Vec<String>> {
    let entries = entries.unwrap_or_default();
    let kept: Vec<String> = entries
        .iter()
        .filter(|entry| {
            let name = if is_modifier(entry) { &entry[1..] } else { entry.as_str() };
            !BUILTIN_TOOLS.contains(&name) && (!cfg!(windows) || name != "powershell")
        })
        .cloned()
        .collect();
    let wanted: Vec<String> = BUILTIN_TOOLS
        .iter()
        .filter(|name| selected.iter().any(|item| item == **name))
        .map(|name| {
            if *name == "bash" && is_powershell_enabled(Some(&resolved(&entries))) {
                "powershell".into()
            } else {
                (*name).into()
            }
        })
        .collect();
    if kept.iter().any(|entry| !is_modifier(entry)) {
        return Some(wanted.into_iter().chain(kept).collect());
    }
    let mut next = kept;
    for name in DEFAULT_TOOLS.iter().filter(|name| !wanted.iter().any(|item| item == **name)) {
        next.push(format!("-{name}"));
    }
    for name in wanted.iter().filter(|name| !DEFAULT_TOOLS.contains(&name.as_str())) {
        next.push(format!("+{name}"));
    }
    (!next.is_empty()).then_some(next)
}

fn is_powershell_enabled(tools: Option<&Vec<String>>) -> bool {
    cfg!(windows)
        && tools.is_some_and(|tools| {
            tools.iter().any(|tool| tool == "powershell") && !tools.iter().any(|tool| tool == "bash")
        })
}

fn codemode_preference(entries: Option<&[String]>) -> &'static str {
    let Some(entries) = entries else { return "automatic" };
    let tools = resolved(entries);
    if tools.iter().any(|tool| tool == "codemode") { "always" } else { "automatic" }
}

fn read_response(settings: &SettingsManager) -> Result<Value, String> {
    let entries = raw_entries(settings)?;
    let tools = entries
        .as_deref()
        .map(resolved)
        .unwrap_or_else(|| DEFAULT_TOOLS.iter().map(|name| (*name).to_owned()).collect());
    let shells: &[&str] = if cfg!(windows) { &["bash", "powershell"] } else { &["bash"] };
    let builtin: Vec<&str> = BUILTIN_TOOLS
        .iter()
        .copied()
        .filter(|name| {
            if *name == "bash" {
                shells.iter().any(|shell| tools.iter().any(|tool| tool == shell))
            } else {
                tools.iter().any(|tool| tool == name)
            }
        })
        .collect();
    let codemode = entries.as_deref().map(|entries| codemode_preference(Some(entries))).unwrap_or("automatic");
    let raw_codemode = settings.global_value("codemode");
    if raw_codemode.as_ref().is_some_and(|value| !value.is_object()) {
        // Reads follow the extension's optional property access: malformed object means defaults.
    }
    let nested = raw_codemode.as_ref().and_then(Value::as_object);
    let raw_mode = nested.and_then(|object| object.get("mode"));
    let mode = if raw_mode.and_then(Value::as_str) == Some("only") {
        json!({"value":"only"})
    } else if raw_mode.is_none() || raw_mode.and_then(Value::as_str) == Some("on") {
        json!({"value":"on"})
    } else {
        json!({"value":"on", "invalid":short_json(raw_mode.unwrap())})
    };
    let raw_budget = nested.and_then(|object| object.get("inlineBudget"));
    let budget = match raw_budget {
        None => json!({}),
        Some(Value::Number(number)) if number.as_f64().is_some_and(|value| value.is_finite() && value >= 0.0) => {
            json!({"value":number})
        }
        Some(value) => json!({"invalid":short_json(value)}),
    };
    Ok(
        json!({"isWindows":cfg!(windows), "powerShellEnabled":is_powershell_enabled(Some(&tools)), "builtinTools":builtin,
        "codemode":codemode, "codemodeMode":mode, "codemodeInlineBudget":budget}),
    )
}

fn short_json(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 60 { format!("{}…", text.chars().take(59).collect::<String>()) } else { text }
}

pub async fn get(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let settings = manager(&state);
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    match read_response(&settings) {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

fn save(state: &AppState, body: Value) -> Response {
    let Some(object) = body.as_object() else { return failure(StatusCode::BAD_REQUEST, "Expected a JSON object") };
    let fields = ["enabled", "builtinTools", "codemode", "codemodeMode", "codemodeInlineBudget"];
    let present: Vec<_> = fields.iter().filter(|field| object.contains_key(**field)).collect();
    if present.len() != 1 {
        return failure(StatusCode::BAD_REQUEST, "Send exactly one settings field");
    }
    let settings = manager(state);
    let change = *present[0];
    match change {
        "builtinTools" => {
            let Some(values) = object["builtinTools"].as_array().filter(|values| values.iter().all(Value::is_string))
            else {
                return failure(StatusCode::BAD_REQUEST, "builtinTools must be an array of strings");
            };
            if values.iter().any(|value| !BUILTIN_TOOLS.contains(&value.as_str().unwrap_or_default())) {
                return failure(StatusCode::BAD_REQUEST, "Invalid builtinTools");
            }
            let entries = match raw_entries(&settings) {
                Ok(entries) => entries,
                Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, error),
            };
            settings.set_global_value(
                "defaultTools",
                with_builtin_tools(
                    entries,
                    &values.iter().filter_map(|value| value.as_str().map(str::to_owned)).collect::<Vec<_>>(),
                )
                .map(|entries| json!(entries)),
            );
        }
        "enabled" => {
            if !cfg!(windows) {
                return failure(StatusCode::NOT_FOUND, "PowerShell tool settings are only available on Windows");
            }
            let Some(enabled) = object["enabled"].as_bool() else {
                return failure(StatusCode::BAD_REQUEST, "enabled must be a boolean");
            };
            let tools = match raw_entries(&settings) {
                Ok(entries) => entries
                    .map(|entries| resolved(&entries))
                    .unwrap_or_else(|| DEFAULT_TOOLS.iter().map(|name| (*name).to_owned()).collect()),
                Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, error),
            };
            let shell = if enabled { "powershell" } else { "bash" };
            let mut next = Vec::new();
            for name in &tools {
                let name = if name == "bash" || name == "powershell" { shell } else { name };
                if !next.iter().any(|item| item == name) {
                    next.push(name.to_owned());
                }
            }
            if !tools.iter().any(|name| name == "bash" || name == "powershell") {
                next.push(shell.into());
            }
            settings.set_global_value("defaultTools", Some(json!(next)));
        }
        "codemode" => {
            let Some(preference) = object["codemode"].as_str().filter(|value| ["automatic", "always"].contains(value))
            else {
                return failure(StatusCode::BAD_REQUEST, "codemode must be automatic or always");
            };
            let entries = match raw_entries(&settings) {
                Ok(entries) => entries,
                Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, error),
            }
            .unwrap_or_default();
            let kept: Vec<String> = entries
                .into_iter()
                .filter(|entry| (if is_modifier(entry) { &entry[1..] } else { entry.as_str() }) != "codemode")
                .collect();
            let next = if preference == "always" {
                let mut next = kept;
                next.push("+codemode".into());
                Some(next)
            } else if !kept.is_empty() {
                Some(kept)
            } else {
                None
            };
            settings.set_global_value("defaultTools", next.map(|entries| json!(entries)));
        }
        "codemodeMode" => {
            let Some(mode) = object["codemodeMode"].as_str().filter(|value| ["on", "only"].contains(value)) else {
                return failure(StatusCode::BAD_REQUEST, "codemodeMode must be on or only");
            };
            let raw = settings.global_value("codemode");
            if raw.as_ref().is_some_and(|value| !value.is_object()) {
                return failure(StatusCode::INTERNAL_SERVER_ERROR, "Invalid settings.json: codemode must be an object");
            }
            if mode == "on"
                && raw
                    .as_ref()
                    .and_then(Value::as_object)
                    .is_some_and(|object| object.len() == 1 && object.contains_key("mode"))
            {
                settings.set_global_value("codemode", None);
            } else {
                settings.set_global_nested("codemode", "mode", (mode == "only").then(|| json!("only")));
            }
        }
        "codemodeInlineBudget" => {
            let value = &object["codemodeInlineBudget"];
            let valid_budget = value.as_f64().is_some_and(|number| {
                number.is_finite() && number.fract() == 0.0 && (0.0..=1_000_000.0).contains(&number)
            });
            let budget = if value.is_null() {
                None
            } else if valid_budget {
                Some(value.clone())
            } else {
                return failure(
                    StatusCode::BAD_REQUEST,
                    "codemodeInlineBudget must be a whole number from 0 to 1000000, or null",
                );
            };
            let raw = settings.global_value("codemode");
            if raw.as_ref().is_some_and(|value| !value.is_object()) {
                return failure(StatusCode::INTERNAL_SERVER_ERROR, "Invalid settings.json: codemode must be an object");
            }
            if budget.is_none()
                && raw
                    .as_ref()
                    .and_then(Value::as_object)
                    .is_some_and(|object| object.len() == 1 && object.contains_key("inlineBudget"))
            {
                settings.set_global_value("codemode", None);
            } else {
                settings.set_global_nested("codemode", "inlineBudget", budget);
            }
        }
        _ => return failure(StatusCode::BAD_REQUEST, "Invalid settings field"),
    }
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    match read_response(&settings) {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

pub async fn put(axum::extract::State(state): axum::extract::State<AppState>, Json(body): Json<Value>) -> Response {
    save(&state, body)
}

pub fn set_cache_miss(state: &AppState, enabled: bool) -> Result<bool, String> {
    if !enabled && !state.agent_dir.join("settings.json").exists() {
        return Ok(false);
    }
    let settings = manager(state);
    if enabled {
        settings.set_global_value("showCacheMissNotices", Some(Value::Bool(true)));
    } else {
        settings.set_global_value("showCacheMissNotices", None);
    }
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return Err(error.message);
    }
    Ok(settings.global_value("showCacheMissNotices").as_ref().and_then(Value::as_bool) == Some(true))
}

pub async fn cache_get(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let settings = manager(&state);
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    Json(json!({"enabled":settings.global_value("showCacheMissNotices").as_ref().and_then(Value::as_bool) == Some(true)})).into_response()
}

pub async fn cache_put(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
        return failure(StatusCode::BAD_REQUEST, "enabled must be a boolean");
    };
    match set_cache_miss(&state, enabled) {
        Ok(enabled) => Json(json!({"enabled":enabled})).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}
