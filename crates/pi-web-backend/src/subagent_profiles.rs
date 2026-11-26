//! Subagent profile discovery and configuration-file CRUD.
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::web_backend::AppState;

const DEFAULT_TOOLS: &[&str] = &["read", "bash", "edit", "write", "grep", "find", "ls"];
const READ_TOOLS: &[&str] = &["read", "grep", "find", "ls"];
const BUILTIN_TOOLS: &[&str] = &["read", "bash", "edit", "write", "grep", "find", "ls", "powershell", "todo"];
const OWNED_ALIASES: &[&str] = &["none", "all", "true", "false"];
const MANAGED: &[&str] = &[
    "description",
    "display_name",
    "tools",
    "load_skills",
    "load_extensions",
    "codemode",
    "mcp",
    "enabled",
    "inherit_context",
    "run_in_background",
    "model",
    "thinking",
    "max_turns",
    "prompt_mode",
    "color",
    "isolation",
    "persist_session",
    "name",
    "disallowedTools",
    "maxTurns",
    "systemPromptMode",
    "exposure",
];
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Deserialize)]
pub struct CwdQuery {
    cwd: Option<String>,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into()}))).into_response()
}
fn value_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty())
}
fn values(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => {
            items.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
        }
        Some(Value::String(text)) => {
            text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
        }
        _ => Vec::new(),
    }
}
fn name_list(value: Option<&Value>) -> Option<Vec<String>> {
    let Some(value) = value else { return None };
    if let Some(text) = value.as_str() {
        if OWNED_ALIASES.contains(&text.trim().to_lowercase().as_str()) {
            return None;
        }
    }
    let mut seen = BTreeSet::new();
    let names = values(Some(value)).into_iter().filter(|name| seen.insert(name.clone())).collect::<Vec<_>>();
    (!names.is_empty() || value.is_array()).then_some(names)
}
fn bool_value(value: Option<&Value>, fallback: bool) -> bool {
    value.and_then(Value::as_bool).unwrap_or(fallback)
}
fn resource_switch(value: Option<&Value>, fallback: bool) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) if ["none", "false"].contains(&text.trim().to_lowercase().as_str()) => false,
        Some(Value::String(_) | Value::Array(_)) => true,
        _ => fallback,
    }
}
fn parse_profile(file: &Path, scope: &str) -> Option<Value> {
    let source = fs::read_to_string(file).ok()?;
    let parsed = pi_coding_agent::resources::frontmatter::parse_frontmatter(&source).ok()?;
    let data = parsed.frontmatter;
    let filename = file.file_stem()?.to_string_lossy().to_string();
    let name = value_str(data.get("name")).unwrap_or(&filename).to_owned();
    if !valid_name(&name) {
        return None;
    }
    let raw_tools = values(data.get("tools"));
    let tools = if raw_tools.is_empty() {
        DEFAULT_TOOLS.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
    } else {
        raw_tools.clone()
    };
    let denied = values(data.get("disallowed_tools"));
    let denied_extensions =
        denied.iter().filter(|tool| tool.to_lowercase().starts_with("ext:")).cloned().collect::<Vec<_>>();
    let normalize_selector = |selector: &str| {
        let selector = selector.trim();
        let body = if selector.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case("ext:")) {
            &selector[4..]
        } else {
            selector
        };
        body.trim().trim_end_matches('/').trim_end_matches("/*").to_lowercase()
    };
    let extension_tools = raw_tools
        .iter()
        .filter(|tool| tool.to_lowercase().starts_with("ext:"))
        .filter(|tool| {
            let allowed = normalize_selector(tool);
            !denied_extensions.iter().any(|deny| {
                let denied = normalize_selector(deny);
                denied == "*" || denied == allowed || allowed.starts_with(&format!("{denied}/"))
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let skill_value = data.get("load_skills").or_else(|| data.get("skills"));
    let extension_value = data.get("load_extensions").or_else(|| data.get("extensions"));
    let skills = name_list(data.get("skills"));
    let extensions = name_list(data.get("extensions")).filter(|list| !list.iter().any(|s| s == "*"));
    let mut result = json!({
        "name":name,
        "displayName":value_str(data.get("display_name")).unwrap_or(&name),
        "description":value_str(data.get("description")).unwrap_or(&name),
        "systemPrompt":parsed.body,
        "tools":tools.into_iter().filter(|tool| !denied.contains(tool)).collect::<Vec<_>>(),
        "loadSkills":resource_switch(skill_value, false),
        "loadExtensions":resource_switch(extension_value, !extension_tools.is_empty()),
        "codemode":if data.get("codemode").and_then(Value::as_bool)==Some(true) || value_str(data.get("codemode"))==Some("on") {"on"} else if value_str(data.get("codemode"))==Some("inherit") {"inherit"} else {"off"},
        "mcp":if data.get("mcp").and_then(Value::as_bool)==Some(true) || value_str(data.get("mcp"))==Some("on") {"on"} else {"off"},
        "promptMode":if value_str(data.get("prompt_mode"))==Some("replace") {"replace"} else {"append"},
        "inheritContext":bool_value(data.get("inherit_context"), false),
        "runInBackground":bool_value(data.get("run_in_background"), false),
        "enabled":bool_value(data.get("enabled"), true),
        "scope":scope,
        "filePath":file.to_string_lossy(),
    });
    if let Some(map) = result.as_object_mut() {
        if let Some(list) = skills {
            map.insert("skills".into(), json!(list));
        }
        if let Some(list) = extensions {
            map.insert("extensions".into(), json!(list));
        }
        if !extension_tools.is_empty() {
            map.insert("extensionTools".into(), json!(extension_tools));
        }
        if !denied_extensions.is_empty() {
            map.insert("disallowedExtensionTools".into(), json!(denied_extensions));
        }
        if !denied.is_empty() {
            map.insert("disallowedTools".into(), json!(denied));
        }
        if let Some(v) = value_str(data.get("model")) {
            map.insert("model".into(), json!(v));
        }
        if let Some(v) = value_str(data.get("thinking")) {
            if ["off", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&v) {
                map.insert("thinking".into(), json!(v));
            }
        }
        if let Some(v) = data
            .get("max_turns")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n > 0.0)
            .map(|n| n.floor() as u64)
            .filter(|n| *n > 0)
        {
            map.insert("maxTurns".into(), json!(v));
        }
        for (source, target) in [("color", "color"), ("exposure", "exposure"), ("isolation", "isolation")] {
            if let Some(v) = value_str(data.get(source)) {
                map.insert(target.into(), json!(v));
            }
        }
        if let Some(v) = data.get("persist_session").and_then(Value::as_bool) {
            map.insert("persistSession".into(), json!(v));
        }
    }
    Some(result)
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && name.as_bytes()[0].is_ascii_alphanumeric()
}
fn validate_cwd(state: &AppState, cwd: Option<&str>) -> Result<PathBuf, Response> {
    let Some(cwd) = cwd.filter(|s| !s.trim().is_empty()) else {
        return Err(error(StatusCode::BAD_REQUEST, "Valid cwd required"));
    };
    let path = PathBuf::from(cwd);
    let Ok(real) = path.canonicalize() else { return Err(error(StatusCode::BAD_REQUEST, "Valid cwd required")) };
    if !real.is_dir() {
        return Err(error(StatusCode::BAD_REQUEST, "Valid cwd required"));
    }
    // Use the same root set as model and workspace APIs. In particular, the
    // directory picker registers newly selected workspaces in-memory; omitting
    // those roots here made subagent profiles return 403 until a session existed.
    if crate::web_backend::path_access::allowed_existing_path(state, &real).is_none() {
        return Err(error(StatusCode::FORBIDDEN, "Access denied"));
    }
    Ok(real)
}
fn profile_dirs(state: &AppState, cwd: &Path) -> [(PathBuf, &'static str); 3] {
    [
        (state.agent_dir.join("agents"), "global"),
        (cwd.join(".agents").join("agents"), "workspace"),
        (cwd.join(".pi").join("agents"), "project"),
    ]
}
fn read_settings(path: &Path) -> Map<String, Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}
fn builtins(state: &AppState) -> Vec<Value> {
    let disabled = read_settings(&state.agent_dir.join("agents/settings.json"))
        .get("disabledBuiltIns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_enabled = |name: &str| !disabled.iter().filter_map(Value::as_str).any(|s| s.eq_ignore_ascii_case(name));
    let defaults = DEFAULT_TOOLS.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let read_only = READ_TOOLS.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    vec![
        json!({"name":"general-purpose","displayName":"General purpose","description":"Handle a focused implementation or investigation task","systemPrompt":"Work autonomously on the delegated task. Keep the final answer concise and include important files, decisions, and remaining risks.","tools":defaults,"loadSkills":false,"loadExtensions":false,"codemode":"off","mcp":"off","promptMode":"append","inheritContext":false,"runInBackground":false,"enabled":is_enabled("general-purpose"),"scope":"builtin"}),
        json!({"name":"explore","displayName":"Explore","description":"Quickly inspect a codebase without modifying it","systemPrompt":"Explore the codebase to answer the delegated question. Do not modify files. Report concrete findings with file paths and relevant symbols.","tools":read_only,"loadSkills":false,"loadExtensions":false,"codemode":"off","mcp":"off","promptMode":"append","inheritContext":false,"runInBackground":false,"enabled":is_enabled("explore"),"scope":"builtin"}),
        json!({"name":"plan","displayName":"Plan","description":"Design an implementation plan without modifying it","systemPrompt":"Produce an implementation-ready plan for the delegated task. Inspect the repository as needed, do not modify files, and call out dependencies, risks, and verification steps.","tools":READ_TOOLS,"loadSkills":false,"loadExtensions":false,"codemode":"off","mcp":"off","promptMode":"append","inheritContext":false,"runInBackground":false,"enabled":is_enabled("plan"),"scope":"builtin"}),
    ]
}
pub async fn list(State(state): State<AppState>, Query(query): Query<CwdQuery>) -> Response {
    let cwd = match validate_cwd(&state, query.cwd.as_deref()) {
        Ok(cwd) => cwd,
        Err(response) => return response,
    };
    let mut profiles = builtins(&state);
    for (dir, scope) in profile_dirs(&state, &cwd) {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|ft| ft.is_file())
                    && path.extension().and_then(|s| s.to_str()) == Some("md")
                {
                    if scope == "global" || path.canonicalize().is_ok_and(|p| p.starts_with(&cwd)) {
                        if let Some(profile) = parse_profile(&path, scope) {
                            profiles.push(profile);
                        }
                    }
                }
            }
        }
    }
    profiles.sort_by(|a, b| {
        a["displayName"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["displayName"].as_str().unwrap_or("").to_lowercase())
    });
    Json(json!({"profiles":profiles})).into_response()
}

fn profile_list(state: &AppState, cwd: &Path) -> Vec<Value> {
    let mut profiles = builtins(state);
    for (dir, scope) in profile_dirs(state, cwd) {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|ft| ft.is_file())
                    && path.extension().and_then(|s| s.to_str()) == Some("md")
                {
                    if scope == "global" || path.canonicalize().is_ok_and(|p| p.starts_with(cwd)) {
                        if let Some(profile) = parse_profile(&path, scope) {
                            profiles.push(profile);
                        }
                    }
                }
            }
        }
    }
    profiles.sort_by(|a, b| {
        a["displayName"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["displayName"].as_str().unwrap_or("").to_lowercase())
    });
    profiles
}

fn validate_profile_dir(state: &AppState, cwd: &Path, scope: &str) -> Result<PathBuf, Response> {
    match scope {
        "global" => Ok(state.agent_dir.join("agents")),
        "project" => Ok(cwd.join(".pi").join("agents")),
        _ => Err(error(StatusCode::BAD_REQUEST, "scope must be global or project")),
    }
}

fn ensure_project_dir(cwd: &Path, dir: &Path) -> Result<(), Response> {
    let mut ancestor = dir;
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            return Err(error(StatusCode::FORBIDDEN, "Agent profile directory is outside the project root"));
        };
        if parent == ancestor {
            return Err(error(StatusCode::FORBIDDEN, "Agent profile directory is outside the project root"));
        }
        ancestor = parent;
    }
    let real = ancestor
        .canonicalize()
        .map_err(|_| error(StatusCode::FORBIDDEN, "Agent profile directory is outside the project root"))?;
    if !real.starts_with(cwd) {
        return Err(error(StatusCode::FORBIDDEN, "Agent profile directory is outside the project root"));
    }
    Ok(())
}

fn save_profile_file(path: &Path, input: &Value) -> Result<(), Response> {
    let _guard = WRITE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner());
    let profile = input
        .get("profile")
        .and_then(Value::as_object)
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "profile required"))?;
    let name =
        profile.get("name").and_then(Value::as_str).map(str::trim).filter(|s| valid_name(s)).ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "Agent name may contain only letters, numbers, dots, underscores, and hyphens",
            )
        })?;
    let existing = fs::read_to_string(path)
        .ok()
        .and_then(|text| pi_coding_agent::resources::frontmatter::parse_frontmatter(&text).ok())
        .map(|p| p.frontmatter)
        .unwrap_or_default();
    let front = existing;
    let tools = profile
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| BUILTIN_TOOLS.contains(s))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let extension_tools = profile
        .get("extensionTools")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_else(|| {
            values(front.get("tools")).into_iter().filter(|s| s.to_lowercase().starts_with("ext:")).collect()
        });
    let mut tool_names = tools.clone();
    for selector in &extension_tools {
        if !tool_names.iter().any(|t| t.eq_ignore_ascii_case(selector)) {
            tool_names.push(selector.clone());
        }
    }
    let mut managed = Map::new();
    let string = |key: &str| {
        profile.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
    };
    managed.insert("name".into(), json!(name));
    managed.insert("description".into(), json!(string("description").unwrap_or_else(|| name.to_owned())));
    managed.insert("tools".into(), json!(if tool_names.is_empty() { "none".into() } else { tool_names.join(", ") }));
    managed.insert("display_name".into(), json!(name));
    let load_skills = profile.get("loadSkills").and_then(Value::as_bool).unwrap_or(false);
    let load_extensions = profile.get("loadExtensions").and_then(Value::as_bool).unwrap_or(false);
    managed.insert("load_skills".into(), json!(load_skills));
    managed.insert("load_extensions".into(), json!(load_extensions));
    managed.insert("codemode".into(), json!(profile.get("codemode").and_then(Value::as_str).unwrap_or("off")));
    if let Some(mcp) = profile.get("mcp").and_then(Value::as_str) {
        if mcp == "on" || mcp == "off" {
            managed.insert("mcp".into(), json!(mcp));
        }
    }
    managed.insert("enabled".into(), json!(profile.get("enabled").and_then(Value::as_bool).unwrap_or(true)));
    managed.insert(
        "inherit_context".into(),
        json!(profile.get("inheritContext").and_then(Value::as_bool).unwrap_or(false)),
    );
    managed.insert(
        "run_in_background".into(),
        json!(profile.get("runInBackground").and_then(Value::as_bool).unwrap_or(false)),
    );
    let prompt_mode =
        if profile.get("promptMode").and_then(Value::as_str) == Some("replace") { "replace" } else { "append" };
    managed.insert("prompt_mode".into(), json!(prompt_mode));
    // Keep unknown metadata and resource alias fields authored by other runtimes.
    for alias in ["skills", "extensions"] {
        if let Some(old) = front.get(alias).cloned() {
            let is_owned = old.as_bool().is_some()
                || old.as_str().is_some_and(|s| OWNED_ALIASES.contains(&s.trim().to_lowercase().as_str()));
            if is_owned {
                managed.insert(alias.into(), json!(if alias == "skills" { load_skills } else { load_extensions }));
            }
        }
    }
    if let Some(max) = profile.get("maxTurns").and_then(Value::as_u64).filter(|n| *n > 0) {
        managed.insert("max_turns".into(), json!(max));
    }
    if let Some(disallowed) = profile.get("disallowedTools").and_then(Value::as_array) {
        if !disallowed.is_empty() {
            managed.insert("disallowed_tools".into(), Value::Array(disallowed.clone()));
        }
    }
    for (source, target) in [
        ("model", "model"),
        ("thinking", "thinking"),
        ("exposure", "exposure"),
        ("color", "color"),
        ("isolation", "isolation"),
    ] {
        if let Some(value) = string(source) {
            managed.insert(target.into(), json!(value));
        }
    }
    if let Some(value) = profile.get("persistSession").and_then(Value::as_bool) {
        managed.insert("persist_session".into(), json!(value));
    }
    let mut output = managed.clone();
    for (key, value) in front.iter() {
        if !MANAGED.contains(&key.as_str()) && !output.contains_key(key) {
            output.insert(key.clone(), value.clone());
        }
    }
    let yaml = serde_yaml_ng::to_string(&Value::Object(output))
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let prompt = profile.get("systemPrompt").and_then(Value::as_str).unwrap_or("").trim();
    atomic_write_text(path, &format!("---\n{}---\n\n{}\n", yaml.trim_end(), prompt))
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))
}

fn atomic_write_text(path: &Path, text: &str) -> Result<(), String> {
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_file_name(format!(".subagent-profile.{}.{}.tmp", std::process::id(), id));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp).map_err(|e| e.to_string())?;
        file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        fs::rename(&temp, path).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub async fn save(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let cwd = match validate_cwd(&state, input.get("cwd").and_then(Value::as_str)) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(scope) = input.get("scope").and_then(Value::as_str) else {
        return error(StatusCode::BAD_REQUEST, "scope must be global or project");
    };
    let dir = match validate_profile_dir(&state, &cwd, scope) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if scope == "project" {
        if let Err(response) = ensure_project_dir(&cwd, &dir) {
            return response;
        }
    }
    let name = input.pointer("/profile/name").and_then(Value::as_str).unwrap_or("").trim().to_owned();
    if !valid_name(&name) {
        return error(
            StatusCode::BAD_REQUEST,
            "Agent name may contain only letters, numbers, dots, underscores, and hyphens",
        );
    }
    let path = dir.join(format!("{name}.md"));
    if let Err(e) = save_profile_file(&path, &input) {
        return e;
    }
    match parse_profile(&path, scope) {
        Some(profile) => (StatusCode::OK, Json(json!({"profile":profile}))).into_response(),
        None => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read saved agent profile"),
    }
}

pub async fn patch(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let cwd = match validate_cwd(&state, input.get("cwd").and_then(Value::as_str)) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(name) = input.get("name").and_then(Value::as_str) else {
        return error(StatusCode::BAD_REQUEST, "name required");
    };
    let Some(enabled) = input.get("enabled").and_then(Value::as_bool) else {
        return error(StatusCode::BAD_REQUEST, "enabled required");
    };
    let Some(scope) = input.get("scope").and_then(Value::as_str) else {
        return error(StatusCode::BAD_REQUEST, "scope must be global, project, or builtin");
    };
    if !["builtin", "global", "project"].contains(&scope) {
        return error(StatusCode::BAD_REQUEST, "scope must be global, project, or builtin");
    }
    let Some(profile) = profile_list(&state, &cwd)
        .into_iter()
        .find(|p| p["scope"] == scope && p["name"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(name)))
    else {
        return error(StatusCode::NOT_FOUND, "Agent profile not found");
    };
    if scope == "builtin" {
        let mut settings = read_settings(&state.agent_dir.join("agents/settings.json"));
        let mut disabled = settings.get("disabledBuiltIns").and_then(Value::as_array).cloned().unwrap_or_default();
        disabled.retain(|v| !v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(name)));
        if !enabled {
            disabled.push(json!(name));
        }
        settings.insert("version".into(), json!(1));
        settings.insert("disabledBuiltIns".into(), Value::Array(disabled));
        let target = state.agent_dir.join("agents/settings.json");
        if let Err(e) = write_json(&target, &Value::Object(settings)) {
            return error(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    }
    if scope == "builtin" {
        let mut profile = profile;
        profile["enabled"] = json!(enabled);
        return (StatusCode::OK, Json(json!({"profile":profile}))).into_response();
    }
    let Some(mut saved) = profile.as_object().cloned() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Invalid profile");
    };
    saved.insert("enabled".into(), json!(enabled));
    let mut payload = input.clone();
    payload["profile"] = Value::Object(saved);
    let dir = match validate_profile_dir(&state, &cwd, scope) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if scope == "project" {
        if let Err(response) = ensure_project_dir(&cwd, &dir) {
            return response;
        }
    }
    let path = dir.join(format!("{name}.md"));
    if let Err(e) = save_profile_file(&path, &payload) {
        return e;
    }
    match parse_profile(&path, scope) {
        Some(profile) => (StatusCode::OK, Json(json!({"profile":profile}))).into_response(),
        None => error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read saved agent profile"),
    }
}

pub async fn delete(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let cwd = match validate_cwd(&state, input.get("cwd").and_then(Value::as_str)) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(name) = input.get("name").and_then(Value::as_str).map(str::trim).filter(|s| valid_name(s)) else {
        return error(StatusCode::BAD_REQUEST, "name required");
    };
    let Some(scope) = input.get("scope").and_then(Value::as_str) else {
        return error(StatusCode::BAD_REQUEST, "scope must be global or project");
    };
    let dir = match validate_profile_dir(&state, &cwd, scope) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if scope == "project" {
        if let Err(response) = ensure_project_dir(&cwd, &dir) {
            return response;
        }
    }
    let path = dir.join(format!("{name}.md"));
    if scope == "project" && path.exists() && !path.canonicalize().is_ok_and(|real| real.starts_with(&cwd)) {
        return error(StatusCode::FORBIDDEN, "Agent profile directory is outside the project root");
    }
    let _guard = WRITE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner());
    if path.exists() {
        if let Err(e) = fs::remove_file(path) {
            return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }
    Json(json!({"ok":true})).into_response()
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    atomic_write_text(path, &serde_json::to_string_pretty(value).map_err(|e| e.to_string())?)
}
