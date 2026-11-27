use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_ai::auth::store_lock::FileLock;
use pi_coding_agent::resources::trust::ProjectTrustStore;
use pi_mcp::config::{McpExposure, project_mcp_config};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{AppState, failure};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);
static FRESH_TRUST_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();

fn path_in_roots(path: &Path, roots: &[PathBuf]) -> bool {
    let Ok(path) = path.canonicalize() else { return false };
    roots.iter().filter_map(|root| root.canonicalize().ok()).any(|root| path.starts_with(root))
}

fn config_path(
    state: &AppState,
    scope: &str,
    cwd: Option<&str>,
    roots: &[PathBuf],
    allow_untrusted: bool,
) -> Result<PathBuf, Response> {
    if scope == "global" {
        return Ok(state.agent_dir.join("mcp.json"));
    }
    let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) else {
        return Err(failure(StatusCode::BAD_REQUEST, "cwd required for project MCP config"));
    };
    let cwd = PathBuf::from(cwd)
        .canonicalize()
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "Project directory does not exist"))?;
    if !cwd.is_dir() || !roots.iter().filter_map(|root| root.canonicalize().ok()).any(|root| cwd.starts_with(root)) {
        return Err(failure(StatusCode::FORBIDDEN, "Project directory is outside allowed roots"));
    }
    if !allow_untrusted {
        match ProjectTrustStore::new(&state.agent_dir).get(&cwd) {
            Ok(Some(true)) => {}
            Ok(_) => {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({"error":"Project is not trusted","reason":"project-untrusted"})),
                )
                    .into_response());
            }
            Err(error) => {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({"error":error.to_string(),"reason":"trust-unreadable"})),
                )
                    .into_response());
            }
        }
    }
    let path = project_mcp_config(&cwd);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        && fs::canonicalize(&path).is_err()
    {
        return Err((StatusCode::CONFLICT, Json(json!({"error":format!("{}: dangling symbolic link", path.display()),"reason":"link-dangling","path":path}))).into_response());
    }
    if path.exists() && !path_in_roots(&path, roots) {
        return Err((StatusCode::CONFLICT, Json(json!({"error":format!("{} resolves outside allowed roots", path.display()),"reason":"link-outside","path":path}))).into_response());
    }
    Ok(path)
}

fn atomic_edit(
    path: &Path,
    private: bool,
    edit: impl FnOnce(&mut Value) -> Result<bool, String>,
) -> Result<bool, String> {
    #[cfg(not(unix))]
    let _ = private;
    let lock = FileLock::acquire_blocking(path).map_err(|error| format!("{}", error))?;
    let original = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if !private && original.as_ref().is_some_and(|text| text.len() > 1024 * 1024) {
        return Err(format!("{}: larger than 1048576 bytes", path.display()));
    }
    let mut root: Value = match original.as_deref() {
        Some(text) if !text.trim().is_empty() => serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
            .map_err(|error| format!("{}: {error}", path.display()))?,
        _ => json!({}),
    };
    if !root.is_object() || root.get("mcpServers").is_some_and(|servers| !servers.is_object()) {
        return Err(format!("{}: expected an object with an \"mcpServers\" object", path.display()));
    }
    if !edit(&mut root)? {
        return Ok(false);
    }
    let indent = original
        .as_deref()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let trimmed = line.trim_start_matches([' ', '\t']);
                (trimmed.len() < line.len() && !trimmed.is_empty()).then(|| &line[..line.len() - trimmed.len()])
            })
        })
        .unwrap_or("  ");
    let mut bytes = serde_json::to_vec_pretty(&root).map_err(|error| error.to_string())?;
    // serde_json's default pretty formatter uses two spaces. Reformat at the stored indent when it differs.
    if indent != "  " {
        bytes = format_json_with_indent(&root, indent.as_bytes())?;
    }
    bytes.push(b'\n');
    let dest = if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        fs::canonicalize(path).map_err(|_| format!("{}: dangling symbolic link", path.display()))?
    } else {
        path.to_path_buf()
    };
    let parent = dest.parent().ok_or_else(|| "MCP config path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = dest.as_os_str().to_owned();
    temp_name.push(format!(".{}.{}.tmp", std::process::id(), id));
    let temp = PathBuf::from(temp_name);
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(if private { 0o600 } else { 0o644 });
        }
        let mut file = options.open(&temp).map_err(|error| error.to_string())?;
        if !private && let Ok(metadata) = fs::metadata(&dest) {
            file.set_permissions(metadata.permissions()).map_err(|error| error.to_string())?;
        }
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        #[cfg(windows)]
        if dest.exists() {
            fs::remove_file(&dest).map_err(|error| error.to_string())?;
        }
        fs::rename(&temp, &dest).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    drop(lock);
    result.map(|_| true).map_err(|error| format!("{}: {error}", path.display()))
}

fn format_json_with_indent(value: &Value, indent: &[u8]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent);
    let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
    value.serialize(&mut serializer).map_err(|error| error.to_string())?;
    Ok(output)
}

pub async fn edit(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(scope) =
        body.get("scope").and_then(Value::as_str).filter(|scope| *scope == "global" || *scope == "project")
    else {
        return failure(StatusCode::BAD_REQUEST, "scope must be global or project");
    };
    let Some(name) = body.get("name").and_then(Value::as_str).filter(|name| !name.is_empty() && name.len() <= 1024)
    else {
        return failure(StatusCode::BAD_REQUEST, "name must be a server name");
    };
    let roots = body
        .get("allowedRoots")
        .and_then(Value::as_array)
        .map(|roots| roots.iter().filter_map(Value::as_str).map(PathBuf::from).collect::<Vec<_>>())
        .unwrap_or_default();
    let operation = body.get("operation").and_then(Value::as_str).unwrap_or("patch");
    let add = operation == "add";
    let restore = operation == "restore";
    let trust_fresh = body.get("trustFresh").and_then(Value::as_bool) == Some(true);
    if trust_fresh && (scope != "project" || !add) {
        return failure(StatusCode::BAD_REQUEST, "trustFresh is only valid for project add");
    }
    let path = match config_path(&state, scope, body.get("cwd").and_then(Value::as_str), &roots, trust_fresh) {
        Ok(path) => path,
        Err(response) => return response,
    };
    let enabled = body.get("enabled").and_then(Value::as_bool);
    let exposure = body.get("exposure").and_then(Value::as_str).and_then(McpExposure::parse);
    let remove = body.get("remove").and_then(Value::as_bool) == Some(true);
    if !remove && !add && !restore && enabled.is_none() && exposure.is_none() {
        return failure(StatusCode::BAD_REQUEST, "An enabled, exposure, or remove change is required");
    }
    let fresh_lock = if trust_fresh {
        let cwd = body.get("cwd").and_then(Value::as_str).expect("project scope requires cwd");
        let cwd = PathBuf::from(cwd).canonicalize().expect("config_path validated cwd");
        let locks = FRESH_TRUST_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        let lock = locks
            .lock()
            .expect("fresh trust lock map poisoned")
            .entry(cwd.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let guard = lock.lock_owned().await;
        let trust_store = ProjectTrustStore::new(&state.agent_dir);
        let entry = match trust_store.get_entry(&cwd) {
            Ok(entry) => entry,
            Err(error) => {
                return (StatusCode::CONFLICT, Json(json!({"error":error.to_string(),"reason":"trust-unreadable"})))
                    .into_response();
            }
        };
        let breadth = match crate::project_trust::fresh_folder_breadth(&state.agent_dir, &cwd, &roots) {
            Ok(breadth) => breadth,
            Err(error) => {
                return (StatusCode::CONFLICT, Json(json!({"error":error,"reason":"trust-unreadable"})))
                    .into_response();
            }
        };
        if let Some(breadth) = breadth {
            return (StatusCode::CONFLICT, Json(json!({"error":format!("Trusting {} would also trust {}", cwd.display(), breadth.path.display()),"reason":"trust-too-broad","breadth":breadth}))).into_response();
        }
        if entry.is_some()
            || pi_coding_agent::resources::trust::has_trust_requiring_project_resources(
                &cwd,
                &pi_coding_agent::resources::paths::home_dir(),
            )
            || crate::project_trust::folder_has_trust_relevant_entries(&cwd)
        {
            return (StatusCode::CONFLICT, Json(json!({"error":format!("{} now has project resources that need trust or a trust decision", cwd.display()),"reason":"folder-not-fresh"}))).into_response();
        }
        if let Err(error) = trust_store.set(&cwd, Some(true)) {
            return (StatusCode::CONFLICT, Json(json!({"error":error.to_string(),"reason":"trust-unreadable"})))
                .into_response();
        }
        Some((guard, cwd))
    } else {
        None
    };
    let private = scope == "global";
    let mut removed_entry: Option<(usize, Value)> = None;
    let result = if add || restore {
        let Some(entry) =
            body.get("entry").filter(|_| body.as_object().is_some_and(|object| object.contains_key("entry")))
        else {
            return failure(StatusCode::BAD_REQUEST, "entry is required for add or restore");
        };
        let index = body.get("index").and_then(Value::as_u64).map(|index| index as usize);
        atomic_edit(&path, private, |root| {
            if !root.get("mcpServers").is_some_and(Value::is_object) {
                root["mcpServers"] = json!({});
            }
            let servers = root["mcpServers"].as_object_mut().expect("object initialized");
            if servers.contains_key(name) {
                return Err(format!("{} already defines MCP server \"{name}\"", path.display()));
            }
            if let Some(index) = index {
                servers.shift_insert(index.min(servers.len()), name.to_owned(), entry.clone());
            } else {
                servers.insert(name.to_owned(), entry.clone());
            }
            Ok(true)
        })
    } else if remove {
        atomic_edit(&path, private, |root| {
            let servers = root.get_mut("mcpServers").and_then(Value::as_object_mut);
            let Some(servers) = servers else {
                return Err(format!("{} does not define MCP server \"{name}\"", path.display()));
            };
            let Some(index) = servers.keys().position(|key| key == name) else {
                return Err(format!("{} does not define MCP server \"{name}\"", path.display()));
            };
            let Some((_, entry)) = servers.shift_remove_entry(name) else {
                return Err(format!("{} does not define MCP server \"{name}\"", path.display()));
            };
            removed_entry = Some((index, entry));
            Ok(true)
        })
    } else {
        atomic_edit(&path, private, |root| {
            if !root.get("mcpServers").is_some_and(Value::is_object) {
                root["mcpServers"] = json!({});
            }
            let servers = root["mcpServers"].as_object_mut().expect("object initialized");
            if !servers.contains_key(name) && body.get("asOverride").and_then(Value::as_bool) == Some(true) {
                servers.insert(name.to_owned(), json!({}));
            }
            let Some(entry) = servers.get_mut(name) else {
                return Err(format!("{} does not define MCP server \"{name}\"", path.display()));
            };
            if !entry.is_object() {
                return Err(format!(
                    "{} defines MCP server \"{name}\" as something other than an object",
                    path.display()
                ));
            }
            let server = entry.as_object_mut().expect("object checked");
            let keep_defaults =
                !server.contains_key("command") && !server.contains_key("url") && !server.contains_key("type");
            if let Some(enabled) = enabled {
                if enabled && !keep_defaults {
                    server.shift_remove("enabled");
                } else {
                    server.insert("enabled".into(), Value::Bool(enabled));
                }
            }
            if let Some(exposure) = exposure {
                if exposure == McpExposure::Codemode && !keep_defaults {
                    server.shift_remove("exposure");
                } else {
                    server.insert("exposure".into(), Value::String(exposure.as_str().into()));
                }
            }
            Ok(true)
        })
    };
    match result {
        Ok(_) => {
            if let Some((index, entry)) = removed_entry {
                Json(json!({"ok":true,"path":path,"index":index,"entry":entry})).into_response()
            } else if let Some((_guard, cwd)) = fresh_lock {
                Json(json!({"ok":true,"path":path,"trust":{"requiresTrust":true,"trusted":true,"decision":true,"decisionPath":cwd,"inherited":false}})).into_response()
            } else {
                Json(json!({"ok":true,"path":path})).into_response()
            }
        }
        Err(error) => {
            let rollback_error = fresh_lock.as_ref().and_then(|(_guard, cwd)| {
                ProjectTrustStore::new(&state.agent_dir).set(cwd, None).err().map(|error| error.to_string())
            });
            let reason = if error.contains("does not define MCP server") {
                "server-missing"
            } else if error.contains("already defines MCP server") {
                "name-taken"
            } else if error.contains("something other than an object") {
                "entry-not-object"
            } else if error.contains("expected an object with") {
                "invalid-shape"
            } else if error.contains("larger than 1048576 bytes") {
                "too-large"
            } else if error.contains("dangling symbolic link") {
                "link-dangling"
            } else if error.to_lowercase().contains("locked") {
                "locked"
            } else {
                "unparsable"
            };
            let mut response = json!({"error":error,"path":path,"name":name,"reason":reason});
            if let Some(rollback_error) = rollback_error {
                if let Some((_guard, cwd)) = fresh_lock {
                    response["rollbackError"] = json!(rollback_error);
                    response["trust"] = json!({"requiresTrust":true,"trusted":true,"decision":true,"decisionPath":cwd,"inherited":false});
                }
            }
            (StatusCode::CONFLICT, Json(response)).into_response()
        }
    }
}
