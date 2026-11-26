use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use super::{config_edit, config_read, test, ui_state};
use crate::web_backend::AppState;

const MAX_BULK_SERVERS: usize = 500;

fn error(status: StatusCode, reason: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into(),"reason":reason}))).into_response()
}

async fn response_json(response: Response) -> Result<(StatusCode, Value), Response> {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .map_err(|body_error| error(StatusCode::BAD_GATEWAY, "internal", body_error.to_string()))?;
    let body = serde_json::from_slice(&bytes).map_err(|body_error| {
        error(StatusCode::BAD_GATEWAY, "internal", format!("Invalid Rust MCP response: {body_error}"))
    })?;
    Ok((status, body))
}

fn roots(state: &AppState) -> Vec<PathBuf> {
    crate::web_backend::path_access::allowed_roots(state)
}

fn validate_cwd(cwd: &str, roots: &[PathBuf]) -> Result<PathBuf, Response> {
    let raw = PathBuf::from(cwd);
    if !raw.is_absolute() {
        return Err(error(StatusCode::BAD_REQUEST, "cwd-invalid", "cwd must be an absolute path"));
    }
    let Ok(canonical) = raw.canonicalize() else {
        return Err(error(StatusCode::BAD_REQUEST, "cwd-not-directory", "cwd must be an existing directory"));
    };
    if !canonical.is_dir() {
        return Err(error(StatusCode::BAD_REQUEST, "cwd-not-directory", "cwd must be a directory"));
    }
    if !roots.iter().filter_map(|root| root.canonicalize().ok()).any(|root| canonical.starts_with(root)) {
        return Err(error(StatusCode::FORBIDDEN, "cwd-denied", "Access denied"));
    }
    Ok(canonical)
}

async fn overview(state: &AppState, cwd: Option<&str>, roots: &[PathBuf]) -> Result<Value, Response> {
    let query = config_read::SourceQuery {
        cwd: cwd.map(str::to_owned),
        allowed_roots: roots.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
    };
    let (status, body) = response_json(config_read::overview(State(state.clone()), Query(query)).await).await?;
    if !status.is_success() {
        return Err((status, Json(body)).into_response());
    }
    Ok(body)
}

async fn edit(state: AppState, body: Value) -> Result<(StatusCode, Value), Response> {
    response_json(config_edit::edit(State(state), Json(body)).await).await
}

fn scope(value: &Value) -> Option<&str> {
    value.get("scope").and_then(Value::as_str).filter(|scope| matches!(*scope, "global" | "project"))
}

fn server<'a>(overview: &'a Value, scope: &str, name: &str) -> Option<&'a Value> {
    overview.get("servers")?.as_array()?.iter().find(|server| {
        server.get("scope").and_then(Value::as_str) == Some(scope)
            && server.get("name").and_then(Value::as_str) == Some(name)
    })
}

fn server_refusal(server: &Value, name: &str) -> Option<Response> {
    if server.get("notAnObject").and_then(Value::as_bool) == Some(true) {
        return Some(error(
            StatusCode::CONFLICT,
            "entry-not-object",
            format!("MCP server \"{name}\" is not an object"),
        ));
    }
    if server.get("validated").and_then(Value::as_bool) != Some(true) {
        return Some(error(
            StatusCode::CONFLICT,
            "server-invalid",
            server.get("invalidError").and_then(Value::as_str).unwrap_or("MCP server configuration is invalid"),
        ));
    }
    None
}

fn source_problem(overview: &Value, scope: &str) -> Option<Response> {
    let file = overview
        .get("files")?
        .as_array()?
        .iter()
        .find(|file| file.get("scope").and_then(Value::as_str) == Some(scope))?;
    let problem = file.get("problems")?.as_array()?.first()?;
    let reason = problem.get("reason").and_then(Value::as_str)?;
    if reason == "auto-enable-codemode-invalid" {
        return None;
    }
    let message = problem.get("error").and_then(Value::as_str).unwrap_or("MCP config could not be read");
    let path = file.get("path").cloned().unwrap_or(Value::Null);
    let (status, reason) = match reason {
        "unreadable" => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        "too-large" => (StatusCode::CONFLICT, "too-large"),
        "link-dangling" => (StatusCode::CONFLICT, "link-dangling"),
        "link-outside" => (StatusCode::CONFLICT, "link-outside"),
        "invalid-shape" => (StatusCode::CONFLICT, "invalid-shape"),
        "unparsable" => (StatusCode::CONFLICT, "unparsable"),
        "too-many-servers" => (StatusCode::CONFLICT, "too-many-servers"),
        _ => (StatusCode::CONFLICT, "invalid-shape"),
    };
    let mut body = json!({"error":message,"reason":reason});
    if !path.is_null() {
        body["path"] = path;
    }
    Some((status, Json(body)).into_response())
}

async fn ensure_server(
    state: &AppState,
    overview: &Value,
    scope: &str,
    name: &str,
    enabling: bool,
) -> Result<(), Response> {
    if let Some(response) = source_problem(overview, scope) {
        return Err(response);
    }
    let Some(server) = server(overview, scope, name) else {
        return Err(error(StatusCode::CONFLICT, "server-missing", format!("MCP server \"{name}\" is not listed")));
    };
    if let Some(response) = server_refusal(server, name) {
        return Err(response);
    }
    if enabling && server.get("webPasswordField").is_some() {
        return Err(error(
            StatusCode::CONFLICT,
            "web-password",
            format!("\"{name}\" references PI_WEB_PASSWORD, so Pi Web does not turn it on"),
        ));
    }
    let _ = state;
    Ok(())
}

async fn write_edit(
    state: &AppState,
    cwd: Option<&str>,
    roots: &[PathBuf],
    scope: &str,
    name: &str,
    change: Value,
) -> Result<(), Response> {
    let mut request = json!({"scope":scope,"name":name});
    if let Some(object) = change.as_object() {
        request.as_object_mut().unwrap().extend(object.clone());
    }
    if scope == "project" {
        let Some(cwd) = cwd else {
            return Err(error(StatusCode::BAD_REQUEST, "invalid-request", "A project server needs the project's cwd"));
        };
        request["cwd"] = json!(cwd);
        request["allowedRoots"] = json!(roots);
    }
    let (status, body) = edit(state.clone(), request).await?;
    if status.is_success() { Ok(()) } else { Err((status, Json(body)).into_response()) }
}

async fn refreshed(state: &AppState, cwd: Option<&str>, roots: &[PathBuf], extra: Map<String, Value>) -> Response {
    match overview(state, cwd, roots).await {
        Ok(mut body) => {
            if let Some(object) = body.as_object_mut() {
                object.extend(extra);
            }
            Json(body).into_response()
        }
        Err(response) => response,
    }
}

fn action_error(status: StatusCode, body: Value, item: Option<(&str, &str)>) -> Value {
    let mut result = json!({});
    if let Some((scope, name)) = item {
        result["scope"] = json!(scope);
        result["name"] = json!(name);
    }
    for key in ["error", "reason", "path", "name", "breadth", "rollbackError", "trust"] {
        if let Some(value) = body.get(key) {
            result[key] = value.clone();
        }
    }
    if result.get("error").is_none() {
        result["error"] = json!(format!("Rust backend returned HTTP {}", status.as_u16()));
    }
    result
}

fn entry_names(value: Option<&str>) -> Result<Vec<String>, String> {
    let Some(text) = value else { return Ok(Vec::new()) };
    let parsed: Value =
        serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text)).map_err(|error| error.to_string())?;
    let Some(entries) = parsed.get("mcpServers").and_then(Value::as_object) else { return Ok(Vec::new()) };
    Ok(entries.keys().cloned().collect())
}

fn raw_entry(
    state: &AppState,
    cwd: Option<&str>,
    roots: &[PathBuf],
    scope: &str,
    name: &str,
) -> Result<(Value, String), Response> {
    let (global_file, global_text) = config_read::global_file(state);
    let global: Value = global_text
        .as_deref()
        .map(|text| serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text)))
        .transpose()
        .map_err(|parse_error: serde_json::Error| error(StatusCode::CONFLICT, "unparsable", parse_error.to_string()))?
        .unwrap_or_else(|| json!({"mcpServers":{}}));
    let mut source_path = global_file["path"].as_str().unwrap_or_default().to_owned();
    if scope == "global" {
        let Some(value) = global.pointer(&format!("/mcpServers/{}", pointer_escape(name))).cloned() else {
            return Err(error(StatusCode::CONFLICT, "server-missing", format!("MCP server \"{name}\" is not listed")));
        };
        return Ok((value, source_path));
    }
    let Some(cwd) = cwd else {
        return Err(error(StatusCode::BAD_REQUEST, "invalid-request", "A project server needs the project's cwd"));
    };
    let (project_file, project_text) = config_read::project_file(cwd, roots)?;
    source_path = project_file["path"].as_str().unwrap_or_default().to_owned();
    let project: Value = project_text
        .as_deref()
        .map(|text| serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text)))
        .transpose()
        .map_err(|parse_error: serde_json::Error| error(StatusCode::CONFLICT, "unparsable", parse_error.to_string()))?
        .unwrap_or_else(|| json!({"mcpServers":{}}));
    let Some(mut value) = project.pointer(&format!("/mcpServers/{}", pointer_escape(name))).cloned() else {
        return Err(error(StatusCode::CONFLICT, "server-missing", format!("MCP server \"{name}\" is not listed")));
    };
    if value.as_object().is_some_and(|entry| {
        !entry.contains_key("command") && !entry.contains_key("url") && !entry.contains_key("type")
    }) {
        if let Some(base) = global.pointer(&format!("/mcpServers/{}", pointer_escape(name))).and_then(Value::as_object)
        {
            let mut merged = base.clone();
            if let Some(patch) = value.as_object() {
                merged.extend(patch.clone());
            }
            value = Value::Object(merged);
            source_path = project_file["path"].as_str().unwrap_or_default().to_owned();
        }
    }
    Ok((value, source_path))
}

fn pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn suggest_name(name: &str, names: &[String]) -> String {
    let used = names.iter().map(String::as_str).collect::<std::collections::HashSet<_>>();
    if !used.contains(name) {
        return name.to_owned();
    }
    for index in 2.. {
        let next = format!("{name}-{index}");
        if !used.contains(next.as_str()) {
            return next;
        }
    }
    name.to_owned()
}

fn string_set(value: Option<&Value>, field: &str) -> Result<std::collections::HashSet<String>, Response> {
    let Some(value) = value else { return Ok(std::collections::HashSet::new()) };
    let Some(items) = value.as_array() else {
        return Err(error(StatusCode::BAD_REQUEST, "invalid-request", format!("{field} must be an array of strings")));
    };
    if items.len() > 200 || items.iter().any(|item| !item.is_string()) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid-request",
            format!("{field} must contain at most 200 strings"),
        ));
    }
    Ok(items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
}

fn literal_secret_paths(config: &Value) -> Vec<String> {
    fn is_secret_name(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        ["secret", "password", "token", "api_key", "api-key", "apikey", "credential", "authorization"]
            .iter()
            .any(|part| lower.contains(part))
    }
    fn is_reference(value: &str) -> bool {
        value.contains("${") || value.starts_with('!') || value.starts_with('$')
    }
    let mut paths = Vec::new();
    if let Some(env) = config.get("env").and_then(Value::as_object) {
        for (name, value) in env {
            if is_secret_name(name) && value.as_str().is_some_and(|value| !value.is_empty() && !is_reference(value)) {
                paths.push(format!("env.{name}"));
            }
        }
    }
    if let Some(headers) = config.get("headers").and_then(Value::as_object) {
        for (name, value) in headers {
            let Some(value) = value.as_str().filter(|value| !value.is_empty() && !is_reference(value)) else {
                continue;
            };
            if is_secret_name(name)
                || value.to_ascii_lowercase().starts_with("bearer ")
                || value.to_ascii_lowercase().starts_with("basic ")
            {
                paths.push(format!("headers.{name}"));
            }
        }
    }
    if config
        .pointer("/oauth/clientSecret")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty() && !is_reference(value))
    {
        paths.push("oauth.clientSecret".to_owned());
    }
    paths
}

pub async fn action(State(state): State<AppState>, Json(mut body): Json<Value>) -> Response {
    if std::env::var_os("PI_WEB_DISABLE_MCP").is_some() {
        return error(StatusCode::CONFLICT, "mcp-off", "PI_WEB_DISABLE_MCP is set");
    }
    let roots = roots(&state);
    let cwd = body.get("cwd").and_then(Value::as_str).map(str::to_owned);
    let cwd = match cwd.as_deref().map(|cwd| validate_cwd(cwd, &roots)).transpose() {
        Ok(cwd) => cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
        Err(response) => return response,
    };
    if let Some(object) = body.as_object_mut() {
        object.remove("allowedRoots");
    }
    let action = body.get("action").and_then(Value::as_str).unwrap_or_default().to_owned();
    let needs_overview = matches!(
        action.as_str(),
        "enable" | "disable" | "set-exposure" | "set-enabled" | "set-in-project" | "sign-out"
    );
    let before = match needs_overview.then(|| overview(&state, cwd.as_deref(), &roots)) {
        Some(overview) => match overview.await {
            Ok(overview) => overview,
            Err(response) => return response,
        },
        None => Value::Null,
    };
    let mut extra = Map::new();

    match action.as_str() {
        "enable" | "disable" | "set-exposure" => {
            let Some(scope) = scope(&body).map(str::to_owned) else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "scope must be global or project");
            };
            let Some(name) = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 1024)
                .map(str::to_owned)
            else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "name must be a server name");
            };
            let enabling = action == "enable"
                || (action == "set-exposure" && body.get("enabled").and_then(Value::as_bool) == Some(true));
            if let Err(response) = ensure_server(&state, &before, &scope, &name, enabling).await {
                return response;
            }
            let change = match action.as_str() {
                "enable" | "disable" => json!({"enabled":action == "enable"}),
                _ => {
                    let Some(exposure) = body
                        .get("exposure")
                        .and_then(Value::as_str)
                        .filter(|value| matches!(*value, "direct" | "codemode" | "deferred" | "hidden"))
                    else {
                        return error(
                            StatusCode::BAD_REQUEST,
                            "invalid-request",
                            "exposure must be direct, codemode, or deferred",
                        );
                    };
                    json!({"exposure":exposure})
                }
            };
            if let Err(response) = write_edit(&state, cwd.as_deref(), &roots, &scope, &name, change).await {
                return response;
            }
        }
        "set-enabled" => {
            let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "enabled must be a boolean");
            };
            let Some(items) = body
                .get("servers")
                .and_then(Value::as_array)
                .filter(|items| !items.is_empty() && items.len() <= MAX_BULK_SERVERS)
            else {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid-request",
                    format!("servers must contain 1 to {MAX_BULK_SERVERS} entries"),
                );
            };
            let mut refs = Vec::<(String, String)>::new();
            for item in items {
                let Some(scope) = scope(item) else {
                    return error(StatusCode::BAD_REQUEST, "invalid-request", "Each server needs a valid scope");
                };
                let Some(name) =
                    item.get("name").and_then(Value::as_str).filter(|name| !name.is_empty() && name.len() <= 1024)
                else {
                    return error(StatusCode::BAD_REQUEST, "invalid-request", "Each server needs a valid name");
                };
                let key = (scope.to_owned(), name.to_owned());
                if !refs.contains(&key) {
                    refs.push(key);
                }
            }
            let mut results = Vec::with_capacity(refs.len());
            for (scope, name) in refs {
                if let Err(response) = ensure_server(&state, &before, &scope, &name, enabled).await {
                    let status = response.status();
                    let body = response_json(response).await.map(|(_, body)| body).unwrap_or_else(|_| json!({}));
                    results.push(action_error(status, body, Some((&scope, &name))));
                    continue;
                }
                match write_edit(&state, cwd.as_deref(), &roots, &scope, &name, json!({"enabled":enabled})).await {
                    Ok(()) => results.push(json!({"scope":scope,"name":name})),
                    Err(response) => {
                        let status = response.status();
                        let body = response_json(response).await.map(|(_, body)| body).unwrap_or_else(|_| json!({}));
                        results.push(action_error(status, body, Some((&scope, &name))));
                    }
                }
            }
            extra.insert("results".into(), json!(results));
        }
        "set-in-project" => {
            let Some(name) = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 1024)
                .map(str::to_owned)
            else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "name must be a global server name");
            };
            let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "enabled must be a boolean");
            };
            if let Err(response) = ensure_server(&state, &before, "global", &name, enabled).await {
                return response;
            }
            if let Err(response) = write_edit(
                &state,
                cwd.as_deref(),
                &roots,
                "project",
                &name,
                json!({"enabled":enabled,"asOverride":true}),
            )
            .await
            {
                return response;
            }
        }
        "remove" => {
            let Some(scope) = scope(&body).map(str::to_owned) else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "scope must be global or project");
            };
            let Some(name) = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 1024)
                .map(str::to_owned)
            else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "name must be a server name");
            };
            let mut request = json!({"scope":scope,"name":name,"allowedRoots":roots});
            if scope == "project" {
                let Some(cwd) = cwd.as_deref() else {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "invalid-request",
                        "A project server needs the project's cwd",
                    );
                };
                request["cwd"] = json!(cwd);
            }
            let (status, result) =
                match response_json(ui_state::remove_with_undo(State(state.clone()), Json(request)).await).await {
                    Ok(result) => result,
                    Err(response) => return response,
                };
            if !status.is_success() {
                return (status, Json(result)).into_response();
            }
            extra.insert("undo".into(), json!({"scope":scope,"name":name,"token":result["token"],"path":result["path"],"expiresInMs":result["expiresInMs"]}));
        }
        "undo" => {
            let Some(token) =
                body.get("token").and_then(Value::as_str).filter(|token| !token.is_empty() && token.len() <= 100)
            else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "token must be an undo token");
            };
            let mut request = json!({"token":token});
            if let Some(cwd) = cwd.as_deref() {
                request["cwd"] = json!(cwd);
            }
            let (status, result) =
                match response_json(ui_state::restore_undo(State(state.clone()), Json(request)).await).await {
                    Ok(result) => result,
                    Err(response) => return response,
                };
            if !status.is_success() {
                return (status, Json(result)).into_response();
            }
            extra.insert("restored".into(), result["restored"].clone());
        }
        "sign-out" => {
            let Some(scope) = scope(&body).map(str::to_owned) else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "scope must be global or project");
            };
            let Some(name) = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 1024)
                .map(str::to_owned)
            else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "name must be a server name");
            };
            if let Some(response) = source_problem(&before, &scope) {
                return response;
            }
            let Some(info) = server(&before, &scope, &name) else {
                return error(StatusCode::CONFLICT, "server-missing", format!("MCP server \"{name}\" is not listed"));
            };
            if let Some(response) = server_refusal(info, &name) {
                return response;
            }
            let (config, source_path) = match raw_entry(&state, cwd.as_deref(), &roots, &scope, &name) {
                Ok(value) => value,
                Err(response) => return response,
            };
            let resolved = if scope == "project" && info.get("override").is_some() {
                let global_config = match raw_entry(&state, None, &roots, "global", &name) {
                    Ok((value, _)) => value,
                    Err(response) => return response,
                };
                let mut merged = global_config.as_object().cloned().unwrap_or_default();
                if let Some(patch) = config.as_object() {
                    merged.extend(patch.clone());
                }
                Value::Object(merged)
            } else {
                config
            };
            let Ok(validated) = pi_mcp::config::validate_mcp_server_config(&name, &resolved) else {
                return error(StatusCode::CONFLICT, "server-invalid", "MCP server configuration is invalid");
            };
            let Some(url) = validated.url.as_deref().filter(|_| validated.uses_oauth()) else {
                return error(
                    StatusCode::CONFLICT,
                    "sign-in-not-oauth",
                    format!(
                        "MCP server \"{name}\" does not use OAuth: only an HTTP server without an Authorization header does"
                    ),
                );
            };
            if scope == "project" {
                let Some(cwd) = cwd.as_deref() else {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "invalid-request",
                        "A project server needs the project's cwd",
                    );
                };
                let canonical = match validate_cwd(cwd, &roots) {
                    Ok(path) => path,
                    Err(response) => return response,
                };
                if pi_coding_agent::resources::trust::ProjectTrustStore::new(&state.agent_dir)
                    .get(&canonical)
                    .ok()
                    .flatten()
                    != Some(true)
                {
                    return error(
                        StatusCode::FORBIDDEN,
                        "project-untrusted",
                        "The project is not trusted, so its MCP server is not signed out",
                    );
                }
            }
            let (status, removed) =
                match response_json(test::sign_out_mcp_server(Json(json!({"name":name,"url":url}))).await).await {
                    Ok(result) => result,
                    Err(response) => return response,
                };
            if !status.is_success() {
                return (status, Json(removed)).into_response();
            }
            let _ =
                ui_state::forget_status_entry(Json(json!({"scope":scope,"sourcePath":source_path,"name":name}))).await;
            extra.insert("signedOut".into(), json!({"scope":scope,"name":name,"removed":removed["removed"]}));
        }
        "add" => {
            let Some(scope) = scope(&body).map(str::to_owned) else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "scope must be global or project");
            };
            let Some(name) = body
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 1024)
                .map(str::to_owned)
            else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "name must be a server name");
            };
            if !name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
            {
                return error(
                    StatusCode::BAD_REQUEST,
                    "name-invalid",
                    format!("\"{name}\" is not a server name pi accepts: use letters, digits, \"_\" and \"-\""),
                );
            }
            let Some(entry) = body.get("entry").cloned() else {
                return error(StatusCode::BAD_REQUEST, "invalid-request", "entry is required");
            };
            let validated = match pi_mcp::config::validate_mcp_server_config(&name, &entry) {
                Ok(validated) => match serde_json::to_value(validated) {
                    Ok(value) => value,
                    Err(serialize_error) => {
                        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", serialize_error.to_string());
                    }
                },
                Err(message) => return error(StatusCode::CONFLICT, "server-invalid", message),
            };
            let (_, references, password_field) = config_read::config_value_references(&validated);
            if let Some(field) = password_field {
                return (StatusCode::CONFLICT, Json(json!({"error":format!("\"{name}\" references PI_WEB_PASSWORD, so Pi Web does not add it"),"reason":"web-password","name":name,"field":field}))).into_response();
            }
            let raw_pi = body.get("rawPi").and_then(Value::as_bool) == Some(true);
            let chosen_variables = match string_set(body.get("chosenVariables"), "chosenVariables") {
                Ok(values) => values,
                Err(response) => return response,
            };
            let confirmed_variables = match string_set(body.get("confirmHostEnv"), "confirmHostEnv") {
                Ok(values) => values,
                Err(response) => return response,
            };
            if scope == "project" {
                let mut secret_paths = match string_set(body.get("secretPaths"), "secretPaths") {
                    Ok(values) => values.into_iter().collect::<Vec<_>>(),
                    Err(response) => return response,
                };
                for path in literal_secret_paths(&validated) {
                    if !secret_paths.contains(&path) {
                        secret_paths.push(path);
                    }
                }
                if !secret_paths.is_empty() {
                    return (StatusCode::CONFLICT, Json(json!({"error":format!("\"{name}\" holds a literal secret ({}), so it is saved only in the global mcp.json",secret_paths.join(", ")),"reason":"secret-global-only","name":name,"fields":secret_paths}))).into_response();
                }
            }
            if !raw_pi {
                let mut seen = std::collections::HashSet::new();
                let host_variables = references
                    .iter()
                    .filter(|field| field.get("kind").and_then(Value::as_str) != Some("env"))
                    .flat_map(|field| field.get("variables").and_then(Value::as_array).into_iter().flatten())
                    .filter_map(Value::as_str)
                    .filter(|name| !name.eq_ignore_ascii_case("PI_WEB_PASSWORD") && std::env::var_os(name).is_some())
                    .filter(|name| !chosen_variables.contains(*name))
                    .filter(|name| seen.insert((*name).to_owned()))
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let unconfirmed = host_variables
                    .iter()
                    .filter(|name| !confirmed_variables.contains(*name))
                    .cloned()
                    .collect::<Vec<_>>();
                if !unconfirmed.is_empty() {
                    return (StatusCode::CONFLICT, Json(json!({"error":format!("\"{name}\" would send these variables of the Pi Web host to its server: {}",host_variables.join(", ")),"reason":"host-env-confirm","name":name,"names":host_variables,"requiredNames":unconfirmed}))).into_response();
                }
            }
            let names = match scope.as_str() {
                "global" => entry_names(config_read::global_file(&state).1.as_deref()),
                _ => {
                    let Some(cwd) = cwd.as_deref() else {
                        return error(
                            StatusCode::BAD_REQUEST,
                            "invalid-request",
                            "A project server needs the project's cwd",
                        );
                    };
                    match config_read::project_file(cwd, &roots) {
                        Ok((_, text)) => entry_names(text.as_deref()),
                        Err(response) => return response,
                    }
                }
            };
            let names = match names {
                Ok(names) => names,
                Err(message) => return error(StatusCode::CONFLICT, "unparsable", message),
            };
            if names.contains(&name) {
                let response = error(
                    StatusCode::CONFLICT,
                    "name-taken",
                    format!("The file already defines MCP server \"{name}\""),
                );
                let mut body = match response_json(response).await {
                    Ok((_, body)) => body,
                    Err(response) => return response,
                };
                body["name"] = json!(name);
                body["suggestedName"] = json!(suggest_name(&name, &names));
                return (StatusCode::CONFLICT, Json(body)).into_response();
            }
            let mut request =
                json!({"scope":scope,"name":name,"operation":"add","entry":validated,"allowedRoots":roots});
            if let Some(cwd) = cwd.as_deref() {
                request["cwd"] = json!(cwd);
            }
            if body.get("trustFolder").and_then(Value::as_bool) == Some(true) {
                request["trustFresh"] = json!(true);
            }
            let (status, result) = match edit(state.clone(), request).await {
                Ok(result) => result,
                Err(response) => return response,
            };
            if !status.is_success() {
                if result.get("reason").and_then(Value::as_str) == Some("name-taken") {
                    let latest = match scope.as_str() {
                        "global" => entry_names(config_read::global_file(&state).1.as_deref()),
                        _ => cwd
                            .as_deref()
                            .and_then(|cwd| config_read::project_file(cwd, &roots).ok())
                            .and_then(|(_, text)| entry_names(text.as_deref()).ok())
                            .ok_or_else(|| "project MCP config is unavailable".to_owned()),
                    };
                    if let Ok(latest) = latest {
                        let mut response = result;
                        response["name"] = json!(name);
                        response["suggestedName"] = json!(suggest_name(&name, &latest));
                        return (status, Json(response)).into_response();
                    }
                }
                return (status, Json(result)).into_response();
            }
            extra.insert("added".into(), json!({"scope":scope,"name":name,"path":result["path"]}));
            if let Some(trust) = result.get("trust") {
                extra.insert("trust".into(), trust.clone());
                extra.insert("trustedFolder".into(), json!(true));
            } else if scope == "project" {
                if let Some(cwd) = cwd.as_deref() {
                    if let Ok(trust) = crate::web_backend::project_trust::status(&state, Path::new(cwd)) {
                        if trust.get("decisionError").is_none() {
                            extra.insert("trust".into(), trust);
                        }
                    }
                }
            }
        }
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid-request",
                "action must be add, enable, disable, remove, set-enabled, set-exposure, set-in-project, undo or sign-out",
            );
        }
    }
    refreshed(&state, cwd.as_deref(), &roots, extra).await
}
