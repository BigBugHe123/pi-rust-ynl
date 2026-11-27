use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::resources::brand::project_config_path;
use pi_coding_agent::resources::paths::home_dir;
use pi_coding_agent::resources::trust::{ProjectTrustStore, has_trust_requiring_project_resources};
use pi_coding_agent::settings::{SettingsManager, SettingsOptions, SettingsScope};
use pi_mcp::config::project_mcp_config;
use pi_mcp::config::validate_mcp_server_config;
use pi_mcp::runtime::McpOAuthCredentialStore;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AppState, failure};

const MAX_PROJECT_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceQuery {
    pub cwd: Option<String>,
    #[serde(default)]
    pub allowed_roots: Vec<String>,
}

#[derive(Deserialize)]
pub struct AuthStatusRequest {
    servers: Vec<AuthStatusServer>,
}

#[derive(Deserialize)]
pub struct AuthStatusServer {
    name: String,
    url: String,
}

fn within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().filter_map(|root| root.canonicalize().ok()).any(|root| path.starts_with(root))
}

fn open_regular(path: &Path, nonblocking: bool) -> std::io::Result<(File, fs::Metadata)> {
    #[cfg(not(unix))]
    let _ = nonblocking;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    if nonblocking {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0x800 | 0x20000); // O_NONBLOCK | O_NOFOLLOW
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ))]
        options.custom_flags(0x4 | 0x100); // O_NONBLOCK | O_NOFOLLOW
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    Ok((file, metadata))
}

pub(crate) fn global_file(state: &AppState) -> (Value, Option<String>) {
    let path = state.agent_dir.join("mcp.json");
    let mut file = json!({"scope":"global","path":path,"exists":false,"problems":[]});
    let resolved = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (file, None),
        Err(error) => {
            file["exists"] = json!(true);
            file["problems"] = json!([{"reason":"unreadable","error":error.to_string()}]);
            return (file, None);
        }
    };
    if resolved != path {
        file["realPath"] = json!(resolved);
    }
    let text = read_file(&mut file, &resolved, None, true);
    (file, text)
}

pub(crate) fn project_file(cwd: &str, roots: &[PathBuf]) -> Result<(Value, Option<String>), Response> {
    let cwd = PathBuf::from(cwd)
        .canonicalize()
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "Project directory does not exist"))?;
    if !cwd.is_dir() || !within_roots(&cwd, roots) {
        return Err(failure(StatusCode::FORBIDDEN, "Project directory is outside allowed roots"));
    }
    // Match pi-mcp: a linked worktree without a local file reads the corresponding
    // file from its main checkout. The resolved source still has to remain in
    // one of the folders Pi Web is allowed to read below.
    let path = project_mcp_config(&cwd);
    let mut info = json!({"scope":"project","path":path,"exists":false,"problems":[]});
    let resolved = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                info["exists"] = json!(true);
                info["problems"] = json!([{"reason":"link-dangling","error":"a symbolic link to nothing"}]);
            }
            return Ok((info, None));
        }
        Err(error) => {
            info["exists"] = json!(true);
            info["problems"] = json!([{"reason":"unreadable","error":error.to_string()}]);
            return Ok((info, None));
        }
    };
    if !within_roots(&resolved, roots) {
        info["exists"] = json!(true);
        info["realPath"] = json!(resolved);
        info["problems"] =
            json!([{"reason":"link-outside","error":"a symbolic link outside the folders Pi Web may read"}]);
        return Ok((info, None));
    }
    if resolved != path {
        info["realPath"] = json!(resolved);
    }
    let text = read_file(&mut info, &resolved, Some(MAX_PROJECT_BYTES), true);
    Ok((info, text))
}

fn read_file(info: &mut Value, path: &Path, max_bytes: Option<u64>, nonblocking: bool) -> Option<String> {
    let opened = open_regular(path, nonblocking);
    let (mut file, metadata) = match opened {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && max_bytes.is_none() => return None,
        Err(error) => {
            info["exists"] = json!(true);
            info["problems"] = json!([{"reason":"unreadable","error":error.to_string()}]);
            return None;
        }
    };
    info["exists"] = json!(true);
    if !metadata.is_file() {
        info["problems"] = json!([{"reason":"not-a-file","error":"not a regular file"}]);
        return None;
    }
    if max_bytes.is_some_and(|limit| metadata.len() > limit) {
        info["problems"] = json!([{"reason":"too-large","error":format!("larger than {} bytes", max_bytes.unwrap())}]);
        return None;
    }
    let mut bytes = Vec::new();
    let read_result = if let Some(limit) = max_bytes {
        file.take(limit + 1).read_to_end(&mut bytes)
    } else {
        file.read_to_end(&mut bytes)
    };
    if let Err(error) = read_result {
        info["problems"] = json!([{"reason":"unreadable","error":error.to_string()}]);
        return None;
    }
    if max_bytes.is_some_and(|limit| bytes.len() as u64 > limit) {
        info["problems"] = json!([{"reason":"too-large","error":format!("larger than {} bytes", max_bytes.unwrap())}]);
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) => {
            info["problems"] = json!([{"reason":"unreadable","error":error.to_string()}]);
            None
        }
    }
}

pub(crate) fn settings_snapshot(state: &AppState, cwd: Option<&Path>) -> Value {
    let settings_cwd = cwd.unwrap_or(&state.agent_dir);
    let project_trusted = cwd.is_some_and(|cwd| {
        let needs_trust = has_trust_requiring_project_resources(cwd, &home_dir());
        ProjectTrustStore::new(&state.agent_dir)
            .get(cwd)
            .map(|decision| if needs_trust { decision == Some(true) } else { decision.unwrap_or(true) })
            .unwrap_or(false)
    });
    let manager = SettingsManager::create(settings_cwd, &state.agent_dir, SettingsOptions { project_trusted });
    let global_path = state.agent_dir.join("settings.json");
    let mut global = json!({
        "path": global_path,
        "extensions": manager.raw_value("extensions", SettingsScope::Global).unwrap_or(Value::Null),
        "defaultTools": manager.raw_value("defaultTools", SettingsScope::Global).unwrap_or(Value::Null),
        "codemode": manager.raw_value("codemode", SettingsScope::Global).unwrap_or(Value::Null),
    });
    let errors = manager.drain_errors();
    if let Some(error) = errors.iter().find(|error| error.scope == SettingsScope::Global) {
        global["error"] = json!(error.diagnostic());
    }
    let mut snapshot = json!({"global":global});
    if project_trusted {
        let project_path = project_config_path(settings_cwd, "settings.json");
        let mut project = json!({
            "path": project_path,
            "extensions": manager.raw_value("extensions", SettingsScope::Project).unwrap_or(Value::Null),
            "defaultTools": manager.raw_value("defaultTools", SettingsScope::Project).unwrap_or(Value::Null),
            "codemode": manager.raw_value("codemode", SettingsScope::Project).unwrap_or(Value::Null),
        });
        if let Some(error) = errors.iter().find(|error| error.scope == SettingsScope::Project) {
            project["error"] = json!(error.diagnostic());
        }
        snapshot["project"] = project;
    }
    snapshot
}

pub async fn sources(State(state): State<AppState>, Query(query): Query<SourceQuery>) -> Response {
    let roots = if query.allowed_roots.is_empty() {
        crate::path_access::allowed_roots(&state)
    } else {
        query.allowed_roots.iter().map(PathBuf::from).collect::<Vec<_>>()
    };
    let (global_file, global_text) = global_file(&state);
    if let Some(cwd) = query.cwd.filter(|cwd| !cwd.is_empty()) {
        let canonical = match PathBuf::from(&cwd).canonicalize() {
            Ok(path) if path.is_dir() && within_roots(&path, &roots) => path,
            Ok(_) => return failure(StatusCode::FORBIDDEN, "Project directory is outside allowed roots"),
            Err(_) => return failure(StatusCode::BAD_REQUEST, "Project directory does not exist"),
        };
        let settings = settings_snapshot(&state, Some(&canonical));
        match project_file(&cwd, &roots) {
            Ok((project, text)) => {
                Json(json!({"global":{"file":global_file,"text":global_text},"project":{"file":project,"text":text},"settings":settings}))
                    .into_response()
            }
            Err(response) => response,
        }
    } else {
        let settings = settings_snapshot(&state, None);
        Json(json!({"global":{"file":global_file,"text":global_text},"settings":settings})).into_response()
    }
}

/// Produce the Settings → MCP overview in the backend. Config values are only
/// inspected and redacted here; this endpoint never expands variables or runs commands.
pub async fn overview(State(state): State<AppState>, Query(query): Query<SourceQuery>) -> Response {
    let roots = if query.allowed_roots.is_empty() {
        crate::path_access::allowed_roots(&state)
    } else {
        query.allowed_roots.iter().map(PathBuf::from).collect::<Vec<_>>()
    };
    let (global_info, global_text) = global_file(&state);
    let mut files = vec![global_info];
    let mut trusted = false;
    let mut project_settings = None;
    let mut project_text = None;
    let mut project_info = None;
    if let Some(cwd_text) = query.cwd.filter(|cwd| !cwd.is_empty()) {
        let cwd = match PathBuf::from(&cwd_text).canonicalize() {
            Ok(path) if path.is_dir() && within_roots(&path, &roots) => path,
            Ok(_) => return failure(StatusCode::FORBIDDEN, "Project directory is outside allowed roots"),
            Err(_) => return failure(StatusCode::BAD_REQUEST, "Project directory does not exist"),
        };
        let trust_store = ProjectTrustStore::new(&state.agent_dir);
        let required = has_trust_requiring_project_resources(&cwd, &home_dir());
        let trust_result = trust_store.get_entry(&cwd);
        let trust_entry = trust_result.as_ref().ok().and_then(|entry| entry.clone());
        let decision = trust_entry.as_ref().map(|entry| entry.decision);
        trusted = if required { decision == Some(true) } else { decision.unwrap_or(true) };
        project_settings = Some(settings_snapshot(&state, Some(&cwd)));
        match project_file(&cwd_text, &roots) {
            Ok((info, text)) => {
                files.push(info);
                project_text = text;
            }
            Err(response) => return response,
        }
        let mut trust = json!({"trusted":trusted,"requiresTrust":required,"decision":decision,"decisionPath":trust_entry.as_ref().map(|entry|entry.path.clone()),"inherited":trust_entry.as_ref().is_some_and(|entry|entry.path != cwd)});
        if let Err(error) = trust_result {
            trust["decisionError"] = json!(error.to_string());
        }
        let mut info = json!({"cwd":cwd,"trust":trust});
        if !required && decision.is_none() {
            let breadth = crate::project_trust::fresh_folder_breadth(&state.agent_dir, &cwd, &roots).ok().flatten();
            let fresh = trust_entry.is_none()
                && breadth.is_none()
                && !crate::project_trust::folder_has_trust_relevant_entries(&cwd);
            if fresh {
                info["trustFolder"] = json!({"allowed":true});
            } else if let Some(breadth) = breadth {
                info["trustFolder"] = json!({"allowed":false,"reason":"trust-too-broad","breadth":breadth});
            } else {
                info["trustFolder"] = json!({"allowed":false,"reason":"folder-not-fresh"});
            }
        }
        project_info = Some(info);
    }

    let mut servers = Vec::<Value>::new();
    let mut global_configs = serde_json::Map::<String, Value>::new();
    let parsed_project = project_text
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(text)).ok());
    for (index, (scope, text)) in
        [("global", global_text.as_deref()), ("project", project_text.as_deref())].into_iter().enumerate()
    {
        let Some(text) = text else { continue };
        let parsed: Value = match serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text)) {
            Ok(value) => value,
            Err(error) => {
                files[index]["problems"] = json!([{"reason":"unparsable","error":error.to_string()}]);
                continue;
            }
        };
        let Some(object) = parsed.as_object() else {
            files[index]["problems"] =
                json!([{"reason":"invalid-shape","error":"expected an object with an mcpServers object"}]);
            continue;
        };
        if object.get("mcpServers").is_some_and(|value| !value.is_object()) {
            files[index]["problems"] =
                json!([{"reason":"invalid-shape","error":"expected an object with an mcpServers object"}]);
            continue;
        }
        if let Some(auto) = object.get("autoEnableCodemode") {
            if auto.is_boolean() {
                files[index]["autoEnableCodemode"] = auto.clone();
            } else {
                files[index]["problems"] =
                    json!([{"reason":"auto-enable-codemode-invalid","error":"autoEnableCodemode must be a boolean"}]);
            }
        }
        let entries = object.get("mcpServers").and_then(Value::as_object).cloned().unwrap_or_default();
        if index == 1 && entries.len() > 200 {
            files[index]["problems"] = json!([{"reason":"too-many-servers","error":format!("declares {} servers, more than 200", entries.len())}]);
            continue;
        }
        if index == 0 {
            global_configs = entries.clone();
        }
        let source_path = files[index]["path"].as_str().unwrap_or_default().to_owned();
        for (name, raw) in entries {
            if index == 1
                && raw.as_object().is_some_and(|entry| {
                    !entry.contains_key("command") && !entry.contains_key("url") && !entry.contains_key("type")
                })
            {
                if let Some(base) = global_configs.get(&name) {
                    let extra = raw.as_object().is_some_and(|entry| {
                        entry.keys().any(|key| !matches!(key.as_str(), "enabled" | "exposure" | "toolExposure"))
                    });
                    if extra {
                        servers.push(invalid_server(
                            &state,
                            &name,
                            "project",
                            &source_path,
                            &raw,
                            "project override may only set enabled, exposure, and toolExposure",
                        ));
                        continue;
                    }
                    let mut merged = base.as_object().cloned().unwrap_or_default();
                    if let Some(patch) = raw.as_object() {
                        merged.extend(patch.clone());
                    }
                    let merged = Value::Object(merged);
                    servers.push(server_info(&state, &name, "project", &source_path, &merged, Some(&raw), true));
                } else {
                    servers.push(invalid_server(
                        &state,
                        &name,
                        "project",
                        &source_path,
                        &raw,
                        "project override has no global server",
                    ));
                }
                continue;
            }
            let entry = server_info(&state, &name, scope, &source_path, &raw, None, false);
            if index == 0
                && parsed_project
                    .as_ref()
                    .and_then(|p| p.get("mcpServers"))
                    .and_then(Value::as_object)
                    .is_some_and(|entries| entries.contains_key(&name))
            {
                let mut entry = entry;
                let project_entry = parsed_project
                    .as_ref()
                    .and_then(|p| p.get("mcpServers"))
                    .and_then(Value::as_object)
                    .and_then(|entries| entries.get(&name));
                let is_override = project_entry.and_then(Value::as_object).is_some_and(|object| {
                    !object.contains_key("command") && !object.contains_key("url") && !object.contains_key("type")
                });
                if is_override {
                    entry["overriddenByProject"] = json!(true);
                } else {
                    entry["shadowedByProject"] = json!(true);
                }
                servers.push(entry);
            } else {
                let mut entry = entry;
                if index == 1
                    && global_configs.contains_key(&name)
                    && !raw.as_object().is_some_and(|object| {
                        !object.contains_key("command") && !object.contains_key("url") && !object.contains_key("type")
                    })
                {
                    entry["replacesGlobal"] = json!(true);
                }
                if index == 1 && !trusted {
                    entry["enabled"] = json!(false);
                }
                servers.push(entry);
            }
        }
    }
    apply_loader_rules(&mut servers);
    let _ = parsed_project;
    let settings = project_settings.unwrap_or_else(|| settings_snapshot(&state, None));
    let global_settings = settings.get("global").cloned().unwrap_or(Value::Null);
    let project_settings = settings.get("project").cloned();
    let global_extensions = string_values(global_settings.get("extensions"));
    let project_extensions = project_settings.as_ref().map(|settings| string_values(settings.get("extensions")));
    let mcp_global_enabled = builtin_enabled("mcp", &global_extensions);
    let mcp_project_enabled = project_extensions
        .as_ref()
        .and_then(|entries| builtin_project_override("mcp", entries))
        .unwrap_or(mcp_global_enabled);
    let codemode_global_enabled = builtin_enabled("codemode", &global_extensions);
    let codemode_project_enabled = project_extensions
        .as_ref()
        .and_then(|entries| builtin_project_override("codemode", entries))
        .unwrap_or(codemode_global_enabled);
    let tool_search_enabled = project_extensions
        .as_ref()
        .and_then(|entries| builtin_project_override("tool-search", entries))
        .unwrap_or_else(|| builtin_enabled("tool-search", &global_extensions));
    let mcp_disabled = std::env::var_os("PI_WEB_DISABLE_MCP").is_some();
    let availability = if mcp_disabled {
        json!({"available":false,"reason":"operator-disabled","error":"PI_WEB_DISABLE_MCP is set"})
    } else if !mcp_project_enabled {
        json!({"available":false,"reason":"builtin-disabled","error":"the extensions setting turns builtin:mcp off","settingsPath":project_settings.as_ref().map(|s|s["path"].clone()).or_else(||global_settings.get("path").cloned())})
    } else {
        json!({"available":true})
    };
    let default_tools = string_values(global_settings.get("defaultTools"));
    let preference = if pi_coding_agent::settings::tools::resolve_default_tools(&default_tools)
        .iter()
        .any(|entry| entry == "codemode")
    {
        "always"
    } else {
        "automatic"
    };
    let settings_path = global_settings.get("path").cloned().unwrap_or(json!(state.agent_dir.join("settings.json")));
    let mut codemode = json!({"sandbox":sandbox_status().await,"builtinDisabled":!codemode_project_enabled});
    if !codemode_project_enabled {
        codemode["builtinSettingsPath"] = project_settings
            .as_ref()
            .filter(|_| !codemode_global_enabled)
            .and_then(|s| s.get("path").cloned())
            .or_else(|| global_settings.get("path").cloned())
            .unwrap_or(Value::Null);
    }
    if project_settings.is_some() && !codemode_global_enabled {
        codemode["globalBuiltinSettingsPath"] = global_settings.get("path").cloned().unwrap_or(Value::Null);
    }
    if let Some(error) = global_settings.get("error") {
        codemode["preferenceError"] = error.clone();
        codemode["modeError"] = error.clone();
        codemode["inlineBudgetError"] = error.clone();
    } else {
        codemode["preference"] = json!(preference);
        let mode =
            global_settings.pointer("/codemode/mode").and_then(Value::as_str).filter(|v| *v == "only").unwrap_or("on");
        let mut mode_setting = json!({"value":mode,"settingsPath":settings_path});
        if let Some(raw) = global_settings.pointer("/codemode/mode") {
            if raw.as_str().is_none_or(|value| !matches!(value, "on" | "only")) {
                mode_setting["invalid"] = json!(short_json(raw));
            }
        }
        codemode["mode"] = mode_setting;
        let mut budget_setting = json!({"settingsPath":settings_path,"default":3000,"max":1000000});
        if let Some(raw) = global_settings.pointer("/codemode/inlineBudget") {
            if let Some(budget) = raw.as_f64().filter(|value| value.is_finite() && *value >= 0.0) {
                budget_setting["value"] = json!(budget);
            } else {
                budget_setting["invalid"] = json!(short_json(raw));
            }
        }
        codemode["inlineBudget"] = budget_setting;
    }
    if let Some(project) = project_settings.as_ref() {
        if project.get("defaultTools").is_some() {
            let global_tools = string_values(global_settings.get("defaultTools"));
            let project_tools = string_values(project.get("defaultTools"));
            let auto = pi_coding_agent::settings::tools::merge_default_tools(Some(&global_tools), Some(&project_tools))
                .unwrap_or_default();
            let mut always_global = global_tools;
            always_global.push("+codemode".to_owned());
            let always =
                pi_coding_agent::settings::tools::merge_default_tools(Some(&always_global), Some(&project_tools))
                    .unwrap_or_default();
            let auto_has =
                pi_coding_agent::settings::tools::resolve_default_tools(&auto).iter().any(|tool| tool == "codemode");
            let always_has =
                pi_coding_agent::settings::tools::resolve_default_tools(&always).iter().any(|tool| tool == "codemode");
            if auto_has == always_has {
                codemode["projectOverride"] =
                    json!({"settingsPath":project["path"],"preference":if auto_has{"always"}else{"automatic"}});
            }
        }
        if let Some(raw) = project.pointer("/codemode/mode") {
            let mut override_value =
                json!({"value":if raw.as_str()==Some("only"){"only"}else{"on"},"settingsPath":project["path"]});
            if raw.as_str().is_none_or(|mode| !matches!(mode, "on" | "only")) {
                override_value["invalid"] = json!(short_json(raw));
            }
            if codemode.get("mode").is_some() {
                codemode["mode"]["projectOverride"] = override_value;
            }
        }
        if let Some(raw) = project.pointer("/codemode/inlineBudget") {
            let mut override_value = json!({"settingsPath":project["path"]});
            if let Some(budget) = raw.as_f64().filter(|value| value.is_finite() && *value >= 0.0) {
                override_value["value"] = json!(budget);
            } else {
                override_value["invalid"] = json!(short_json(raw));
            }
            if codemode.get("inlineBudget").is_some() {
                codemode["inlineBudget"]["projectOverride"] = override_value;
            }
        }
    }
    let mut response =
        json!({"mcp":availability,"codemode":codemode,"files":files,"servers":servers,"project":project_info});
    if !tool_search_enabled {
        response["toolSearchDisabled"] = json!({"settingsPath":project_settings.as_ref().and_then(|s|s.get("path")).or_else(||global_settings.get("path"))});
    }
    Json(response).into_response()
}

fn short_json(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 60 { format!("{}…", text.chars().take(59).collect::<String>()) } else { text }
}

async fn sandbox_status() -> Value {
    match pi_codemode::CodemodeSandbox::new(Default::default()) {
        Ok(sandbox) => match sandbox.execute("return 1 + 1;", Default::default()).await {
            Ok(result) if result.is_ok() => json!({"state":"available"}),
            Ok(result) => {
                json!({"state":"unavailable","error":result.error().map(|error|error.message.as_str()).unwrap_or("sandbox self-test failed")})
            }
            Err(error) => json!({"state":"unavailable","error":error.to_string()}),
        },
        Err(error) => json!({"state":"unavailable","error":error.to_string()}),
    }
}

fn string_values(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}
fn glob_matches(pattern: &str, value: &str) -> bool {
    let (mut p, mut v, mut star, mut retry) = (0usize, 0usize, None, 0usize);
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == value[v] || pattern[p] == b'?') {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = v;
        } else if let Some(s) = star {
            p = s + 1;
            retry += 1;
            v = retry;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
fn builtin_match(pattern: &str, name: &str, exact: bool) -> bool {
    let target = format!("builtin:{name}");
    let pattern = pattern.replace('\\', "/");
    if exact { pattern == target } else { glob_matches(&pattern, &target) }
}
fn builtin_enabled(name: &str, entries: &[String]) -> bool {
    let mut enabled = true;
    for prefix in ['!', '+', '-'] {
        for entry in entries {
            if let Some(pattern) = entry.strip_prefix(prefix) {
                if builtin_match(pattern, name, prefix != '!') {
                    enabled = prefix == '+';
                }
            }
        }
    }
    enabled
}
fn builtin_project_override(name: &str, entries: &[String]) -> Option<bool> {
    let mut result = None;
    for entry in entries {
        let (prefix, pattern) = entry
            .chars()
            .next()
            .filter(|c| matches!(c, '+' | '!' | '-'))
            .map(|c| (Some(c), &entry[1..]))
            .unwrap_or((None, entry.as_str()));
        if builtin_match(pattern, name, matches!(prefix, Some('+' | '-'))) {
            result = Some(!matches!(prefix, Some('!' | '-')));
        }
    }
    result
}

fn invalid_server(state: &AppState, name: &str, scope: &str, path: &str, raw: &Value, error: &str) -> Value {
    server_base(state, name, scope, path, raw, false, Some(error.to_owned()), None)
}

fn server_info(
    state: &AppState,
    name: &str,
    scope: &str,
    path: &str,
    raw: &Value,
    override_raw: Option<&Value>,
    is_override: bool,
) -> Value {
    let config_value = if let Some(patch) = override_raw {
        let mut merged = raw.as_object().cloned().unwrap_or_default();
        if let Some(values) = patch.as_object() {
            merged.extend(values.clone());
        }
        Value::Object(merged)
    } else {
        raw.clone()
    };
    match validate_mcp_server_config(name, &config_value) {
        Err(error) => server_base(state, name, scope, path, raw, false, Some(error), None),
        Ok(config) => {
            if scope == "project" && !is_override && config.url.is_some() && config.auth.is_some() {
                return server_base(
                    state,
                    name,
                    scope,
                    path,
                    raw,
                    false,
                    Some(format!("server \"{name}\": auth is only allowed in the global mcp.json")),
                    None,
                );
            }
            let env_names =
                config.env.as_ref().map(|values| values.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
            let header_names =
                config.headers.as_ref().map(|values| values.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
            let (command, command_masked) = config.command.as_deref().map(mask_command).unwrap_or_default();
            let (args, args_masked) = config.args.as_ref().map(|values| mask_args(values)).unwrap_or_default();
            let (url, url_masked) = config.url.as_deref().map(mask_url).unwrap_or_default();
            let mut value = json!({
                "name":name,"scope":scope,"sourcePath":path,"configKey":super::ui_state::config_key(&config_value),
                "enabled":config.is_enabled(),"validated":true,"transport":if config.url.is_some(){"http"}else{"stdio"},
                "exposure":config.exposure(),"description":config.description,"command":command,"args":args,
                "cwd":config.cwd,"envNames":env_names,"url":url,"headerNames":header_names,
                "usesOAuth":config.uses_oauth(),"authProvider":config.auth_provider(),"commandFields":[],"variableReferences":[],"masked":command_masked || args_masked || url_masked,
            });
            let (command_fields, variable_references, password_field) = config_value_references(&config_value);
            value["commandFields"] = json!(command_fields);
            value["variableReferences"] = json!(variable_references);
            if let Some(field) = password_field {
                value["webPasswordField"] = field;
            }
            if !config.tool_exposures().is_empty() {
                value["toolExposureCount"] = json!(config.tool_exposures().len());
            }
            if config.uses_oauth() {
                if let Some(url) = &config.url {
                    if let Ok(statuses) =
                        McpOAuthCredentialStore::new(&state.agent_dir).statuses(&[(name.to_owned(), url.clone())])
                    {
                        if let Some((signed_in, stored)) = statuses.first() {
                            value["signedIn"] = json!(signed_in);
                            value["oauthStateStored"] = json!(stored);
                        }
                    }
                }
            }
            if let Some(status) =
                super::ui_state::lookup_status(scope, path, name, value["configKey"].as_str().unwrap_or_default())
            {
                value["status"] = status;
            }
            if is_override {
                let keys = override_raw
                    .and_then(Value::as_object)
                    .map(|object| {
                        object
                            .keys()
                            .filter(|key| matches!(key.as_str(), "enabled" | "exposure" | "toolExposure"))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                value["override"] = json!({"keys":keys});
            }
            value
        }
    }
}

fn namespace(name: &str) -> String {
    format!("mcp__{}", name.replace('-', "_"))
}

/// Keep the same first-loaded namespace and project auth rules as Pi's MCP host.
fn apply_loader_rules(servers: &mut [Value]) {
    let mut loaded = Vec::<String>::new();
    for server in servers {
        if server.get("validated").and_then(Value::as_bool) != Some(true) || server.get("override").is_some() {
            continue;
        }
        let Some(name) = server.get("name").and_then(Value::as_str).map(str::to_owned) else { continue };
        if server.get("scope").and_then(Value::as_str) == Some("project")
            && server.get("transport").and_then(Value::as_str) == Some("http")
            && server.get("authProvider").is_some()
        {
            server["validated"] = json!(false);
            server["invalidError"] = json!(format!("server \"{name}\": auth is only allowed in the global mcp.json"));
            continue;
        }
        if let Some(other) = loaded.iter().find(|other| **other != name && namespace(other) == namespace(&name)) {
            server["validated"] = json!(false);
            server["invalidError"] = json!(format!("server \"{name}\" conflicts with \"{other}\""));
            continue;
        }
        loaded.push(name);
    }
}

pub(crate) fn config_value_references(config: &Value) -> (Vec<Value>, Vec<Value>, Option<Value>) {
    let http = config.get("url").is_some();
    let mut values = Vec::<(Value, String)>::new();
    if !http {
        if let Some(env) = config.get("env").and_then(Value::as_object) {
            for (name, value) in env {
                if let Some(value) = value.as_str() {
                    values.push((json!({"kind":"env","name":name}), value.to_owned()));
                }
            }
        }
    } else {
        if let Some(headers) = config.get("headers").and_then(Value::as_object) {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    values.push((json!({"kind":"header","name":name}), value.to_owned()));
                }
            }
        }
        if let Some(value) = config.pointer("/oauth/clientSecret").and_then(Value::as_str) {
            values.push((json!({"kind":"oauth-client-secret"}), value.to_owned()));
        }
    }
    let mut commands = Vec::new();
    let mut references = Vec::new();
    let mut password = None;
    for (field, value) in values {
        let is_command = value.starts_with('!');
        let variables = if is_command {
            if value.to_ascii_uppercase().contains("PI_WEB_PASSWORD") {
                password = Some(field.clone());
            }
            Vec::new()
        } else {
            extract_variables(&value)
        };
        if is_command {
            commands.push(field.clone());
        } else if !variables.is_empty() {
            references.push(json!({"kind":field["kind"],"name":field.get("name"),"variables":variables}));
        }
        if variables.iter().any(|name| name.eq_ignore_ascii_case("PI_WEB_PASSWORD")) {
            password = Some(field);
        }
    }
    (commands, references, password)
}
pub(crate) fn extract_variables(value: &str) -> Vec<String> {
    let bytes = value.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && (bytes[i + 1] == b'$' || bytes[i + 1] == b'!') {
            i += 2;
            continue;
        }
        let (start, end) = if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            if let Some(close) = value[start..].find('}') {
                (start, start + close)
            } else {
                i += 1;
                continue;
            }
        } else {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            (start, end)
        };
        if end > start && bytes[start].is_ascii_alphabetic() || end > start && bytes[start] == b'_' {
            let name = &value[start..end];
            if name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && !found.iter().any(|n| n == name) {
                found.push(name.to_owned());
            }
        }
        i = end.saturating_add(usize::from(bytes.get(end) == Some(&b'}')));
    }
    found
}

const MASKED_SECRET: &str = "•••";
fn looks_secret_name(value: &str) -> bool {
    let mut words = Vec::new();
    let chars = value.chars().collect::<Vec<_>>();
    let mut current = String::new();
    for (i, ch) in chars.iter().enumerate() {
        if !ch.is_ascii_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if ch.is_ascii_uppercase() && i > 0 && chars[i - 1].is_ascii_lowercase() && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(ch.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    if words.len() > 1
        && matches!(
            words.last().map(String::as_str),
            Some(
                "file"
                    | "path"
                    | "dir"
                    | "url"
                    | "uri"
                    | "endpoint"
                    | "id"
                    | "name"
                    | "type"
                    | "header"
                    | "env"
                    | "var"
                    | "length"
                    | "ttl"
                    | "expiry"
                    | "expires"
                    | "timeout"
                    | "mode"
                    | "format"
            )
        )
    {
        return false;
    }
    words.iter().any(|word| {
        matches!(
            word.as_str(),
            "key"
                | "apikey"
                | "token"
                | "secret"
                | "password"
                | "passwd"
                | "passphrase"
                | "pwd"
                | "pass"
                | "credential"
                | "credentials"
                | "authorization"
                | "bearer"
                | "jwt"
                | "cookie"
                | "signature"
                | "pat"
                | "auth"
                | "sig"
                | "code"
                | "pw"
        ) || ["token", "secret", "password", "passwd", "apikey"]
            .iter()
            .any(|suffix| word.len() > suffix.len() && word.ends_with(suffix))
    })
}
fn looks_secret_value(value: &str) -> bool {
    if value.len() < 24
        || value.chars().any(char::is_whitespace)
        || value.starts_with('$')
        || value.contains('/')
        || value.contains('.')
    {
        return false;
    }
    if [
        "sk-",
        "sk_live_",
        "sk_test_",
        "pk_live_",
        "rk_live_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xox",
        "akia",
        "asia",
        "AIza",
        "ya29.",
        "npm_",
        "hf_",
        "shpat_",
        "shpca_",
        "shppa_",
        "dop_v1_",
        "lin_api_",
    ]
    .iter()
    .any(|prefix| value.starts_with(prefix))
    {
        return true;
    }
    if value.starts_with("eyJ") && value.matches('.').count() == 2 {
        return true;
    }
    value.chars().any(|c| c.is_ascii_digit())
        && value.chars().any(|c| c.is_ascii_alphabetic())
        && value.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<std::collections::HashSet<_>>().len() >= 8
        && !value.contains('-')
        && !value.contains('_')
}
fn mask_value(value: &str) -> (String, bool) {
    let mut out = value.to_owned();
    let mut masked = false;
    let mut words = out.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    let mut secret_next = false;
    for i in 0..words.len() {
        if secret_next && matches!(words[i].to_ascii_lowercase().as_str(), "bearer" | "basic" | "token") {
            continue;
        }
        if secret_next && !words[i].starts_with('-') {
            if !words[i].starts_with("${") && !words[i].starts_with('$') {
                words[i] = MASKED_SECRET.into();
                masked = true;
            }
            secret_next = false;
            continue;
        }
        let word = words[i].clone();
        if let Some((key, value)) = word.split_once('=') {
            if looks_secret_name(key.trim_start_matches('-')) && !value.is_empty() && !value.starts_with('$') {
                words[i] = format!("{key}={MASKED_SECRET}");
                masked = true;
                continue;
            }
        }
        secret_next = (word.starts_with('-') && looks_secret_name(word.trim_start_matches('-')))
            || (word.ends_with(':') && looks_secret_name(word.trim_end_matches(':')))
            || matches!(word.to_ascii_lowercase().as_str(), "authorization:" | "bearer" | "basic" | "token");
    }
    out = words.join(" ");
    if let Some((key, _)) = out.split_once('=') {
        if looks_secret_name(key.trim_start_matches('-')) {
            out = format!("{key}={MASKED_SECRET}");
            masked = true;
        }
    }
    if looks_secret_value(&out) {
        out = MASKED_SECRET.to_owned();
        masked = true;
    }
    (out, masked)
}
fn mask_command(value: &str) -> (String, bool) {
    mask_value(value)
}
fn mask_args(values: &[String]) -> (Vec<String>, bool) {
    let mut masked = false;
    let mut secret_next = false;
    let result = values
        .iter()
        .map(|value| {
            if secret_next && !value.starts_with('-') {
                secret_next = false;
                if value.starts_with('$') {
                    let (next, did_mask) = mask_value(value);
                    masked |= did_mask;
                    return next;
                }
                masked = true;
                return MASKED_SECRET.to_owned();
            }
            secret_next = value.starts_with('-') && looks_secret_name(value.trim_start_matches('-'));
            let (next, did_mask) = mask_value(value);
            masked |= did_mask;
            next
        })
        .collect();
    (result, masked)
}
fn mask_url(value: &str) -> (String, bool) {
    let mut out = value.to_owned();
    let mut masked = false;
    if let Some(scheme) = out.find("://") {
        let authority_start = scheme + 3;
        if let Some(end) = out[authority_start..].find(['/', '?', '#']).map(|offset| authority_start + offset) {
            if let Some(at) = out[authority_start..end].rfind('@') {
                out.replace_range(authority_start..authority_start + at, MASKED_SECRET);
                masked = true;
            }
        } else if let Some(at) = out[authority_start..].rfind('@') {
            out.replace_range(authority_start..authority_start + at, MASKED_SECRET);
            masked = true;
        }
    }
    if let Some(authority_end) = out.find("//").and_then(|start| out[start + 2..].find('/').map(|end| start + 2 + end))
    {
        if let Some(path_end) = out[authority_end..].find(['?', '#']).map(|end| authority_end + end) {
            let segments = out[authority_end..path_end]
                .split('/')
                .map(|segment| {
                    if looks_secret_value(segment) {
                        masked = true;
                        MASKED_SECRET
                    } else {
                        segment
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            out.replace_range(authority_end..path_end, &segments);
        } else {
            let segments = out[authority_end..]
                .split('/')
                .map(|segment| {
                    if looks_secret_value(segment) {
                        masked = true;
                        MASKED_SECRET
                    } else {
                        segment
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            out.replace_range(authority_end.., &segments);
        }
    }
    let mut cursor = 0;
    while cursor < out.len() {
        let delimiter = out[cursor..].find(['?', '&', '#']);
        let Some(delimiter) = delimiter.map(|n| cursor + n) else { break };
        let start = delimiter + 1;
        let end = out[start..].find(['&', '#']).map(|n| start + n).unwrap_or(out.len());
        if let Some((name, _)) = out[start..end].split_once('=') {
            let parameter_value = out[start..end].split_once('=').map(|(_, v)| v).unwrap_or("");
            if (looks_secret_name(name) || looks_secret_value(parameter_value)) && !parameter_value.starts_with('$') {
                if let Some(eq) = out[start..end].find('=') {
                    let value_start = start + eq + 1;
                    out.replace_range(value_start..end, MASKED_SECRET);
                    cursor = value_start + MASKED_SECRET.len();
                    masked = true;
                    continue;
                }
            }
        }
        cursor = end;
    }
    (out, masked)
}

fn server_base(
    state: &AppState,
    name: &str,
    scope: &str,
    path: &str,
    raw: &Value,
    validated: bool,
    error: Option<String>,
    _config: Option<()>,
) -> Value {
    let config_key = super::ui_state::config_key(raw);
    let mut value = json!({"name":name,"scope":scope,"sourcePath":path,"configKey":config_key,"enabled":raw.get("enabled").and_then(Value::as_bool).unwrap_or(true),"validated":validated,"envNames":[],"headerNames":[],"usesOAuth":false,"commandFields":[],"variableReferences":[],"masked":false});
    if !raw.is_object() {
        value["notAnObject"] = json!(true);
    }
    if let Some(error) = error {
        value["invalidError"] = json!(error);
    }
    if let Some(status) = super::ui_state::lookup_status(scope, path, name, &config_key) {
        value["status"] = status;
    }
    let _ = state;
    value
}

pub async fn auth_status(State(state): State<AppState>, Json(request): Json<AuthStatusRequest>) -> Response {
    if request.servers.len() > 1000 {
        return failure(StatusCode::PAYLOAD_TOO_LARGE, "Too many MCP auth status entries");
    }
    let servers = request.servers.into_iter().map(|server| (server.name, server.url)).collect::<Vec<_>>();
    match McpOAuthCredentialStore::new(&state.agent_dir).statuses(&servers) {
        Ok(statuses) => Json(json!({"statuses": statuses.into_iter().map(|(signed_in, stored)| {
            json!({"signedIn": signed_in, "stored": stored})
        }).collect::<Vec<_>>()}))
        .into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}
