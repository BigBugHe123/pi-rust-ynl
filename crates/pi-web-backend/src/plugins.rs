use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::packages::{PackageManager, PackageManagerOptions};
use pi_coding_agent::settings::{ResourceKind, SettingsManager, SettingsOptions, SettingsScope};
use serde_json::{Value, json};

use crate::web_backend::{AppState, failure, project_trust::is_trusted_for_resources};

pub async fn action(state: &AppState, input: Value) -> Response {
    let Some(cwd) = input.get("cwd").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "cwd required");
    };
    let cwd = PathBuf::from(cwd);
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", cwd.display()));
    }
    let Some(action) = input.get("action").and_then(Value::as_str) else {
        return failure(StatusCode::BAD_REQUEST, "action required");
    };
    if !matches!(action, "install" | "remove" | "update") {
        return failure(StatusCode::BAD_REQUEST, "Unsupported plugin package action");
    }
    let trusted = is_trusted_for_resources(&cwd, &state.agent_dir);
    let local = input.get("scope").and_then(Value::as_str) == Some("project");
    if local && !trusted {
        return failure(StatusCode::FORBIDDEN, "Project resources must be trusted before modifying project plugins");
    }
    let settings =
        Arc::new(SettingsManager::create(&cwd, &state.agent_dir, SettingsOptions { project_trusted: trusted }));
    let manager = PackageManager::new(PackageManagerOptions::new(&cwd, &state.agent_dir, settings));
    let source = input.get("source").and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty());
    let result = match action {
        "install" => match source {
            Some(source) => manager.install_and_persist(source, local).await,
            None => return failure(StatusCode::BAD_REQUEST, "source required"),
        },
        "remove" => match source {
            Some(source) => manager.remove_and_persist(source, local).await.map(|_| ()),
            None => return failure(StatusCode::BAD_REQUEST, "source required"),
        },
        "update" => {
            if source.is_none() && !trusted && !manager.settings().packages(SettingsScope::Project).is_empty() {
                return failure(
                    StatusCode::FORBIDDEN,
                    "Project resources must be trusted before updating project plugins",
                );
            }
            manager.update(source).await
        }
        _ => unreachable!(),
    };
    match result {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    }
}

pub fn toggle(state: &AppState, input: Value) -> Response {
    let Some(cwd) = input.get("cwd").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "cwd required");
    };
    let cwd = PathBuf::from(cwd);
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", cwd.display()));
    }
    let Some(action) =
        input.get("action").and_then(Value::as_str).filter(|value| matches!(*value, "enable" | "disable"))
    else {
        return failure(StatusCode::BAD_REQUEST, "Unsupported plugin toggle action");
    };
    let disable = action == "disable";
    let trusted = is_trusted_for_resources(&cwd, &state.agent_dir);
    let settings = SettingsManager::create(&cwd, &state.agent_dir, SettingsOptions { project_trusted: trusted });

    if let Some(path) = input.get("path").and_then(Value::as_str) {
        if !native_extension_installed(&state.agent_dir, path) {
            return failure(StatusCode::NOT_FOUND, "Native extension is not installed");
        }
        let target = path.replace('\\', "/");
        let mut entries = settings.resource_paths(ResourceKind::Extensions, SettingsScope::Global);
        entries.retain(|entry| entry.trim_start_matches(['+', '-', '!']).replace('\\', "/") != target);
        entries.push(format!("{}{path}", if disable { "-" } else { "+" }));
        settings.set_resource_paths(ResourceKind::Extensions, &entries);
        if let Some(error) = settings.drain_errors().into_iter().next() {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
        }
        return Json(json!({"ok":true})).into_response();
    }

    if let Some(packages) = input.get("packages").and_then(Value::as_array) {
        let mut results = Vec::new();
        let mut global = settings.global_value("packages").unwrap_or_else(|| json!([]));
        let mut project = settings.project_value("packages").unwrap_or_else(|| json!([]));
        let mut changed_global = false;
        let mut changed_project = false;
        for package in packages {
            let source = package.get("source").and_then(Value::as_str).map(str::trim).unwrap_or_default();
            let scope =
                if package.get("scope").and_then(Value::as_str) == Some("project") { "project" } else { "global" };
            let error = if source.is_empty() {
                Some("Package source is required".to_owned())
            } else if scope == "project" && !trusted {
                Some("Project resources must be trusted before modifying project plugins".to_owned())
            } else {
                let (entries, changed) = if scope == "project" {
                    (&mut project, &mut changed_project)
                } else {
                    (&mut global, &mut changed_global)
                };
                match toggle_package(entries, source, disable, true) {
                    TogglePackage::Missing => Some("Package is not configured".to_owned()),
                    TogglePackage::Kept => Some(
                        "Has resource filters, which disabling would remove; use the package's own switch".to_owned(),
                    ),
                    TogglePackage::Changed => {
                        *changed = true;
                        None
                    }
                    TogglePackage::Unchanged => None,
                }
            };
            results.push(if let Some(error) = error {
                json!({"source":source,"scope":scope,"error":error})
            } else {
                json!({"source":source,"scope":scope})
            });
        }
        if changed_global {
            settings.set_global_value("packages", Some(global));
        }
        if changed_project {
            if let Err(error) = settings.set_project_value("packages", Some(project)) {
                return failure(StatusCode::INTERNAL_SERVER_ERROR, error);
            }
        }
        if let Some(error) = settings.drain_errors().into_iter().next() {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
        }
        return Json(json!({"results":results})).into_response();
    }

    let Some(source) = input.get("source").and_then(Value::as_str).map(str::trim).filter(|source| !source.is_empty())
    else {
        return failure(StatusCode::BAD_REQUEST, "source required");
    };
    let scope = if input.get("scope").and_then(Value::as_str) == Some("project") {
        SettingsScope::Project
    } else {
        SettingsScope::Global
    };
    if scope == SettingsScope::Project && !trusted {
        return failure(StatusCode::FORBIDDEN, "Project resources must be trusted before modifying project plugins");
    }
    let mut entries = settings.raw_value("packages", scope).unwrap_or_else(|| json!([]));
    let _ = toggle_package(&mut entries, source, disable, false);
    let write = if scope == SettingsScope::Project {
        settings.set_project_value("packages", Some(entries))
    } else {
        settings.set_global_value("packages", Some(entries));
        Ok(())
    };
    if let Err(error) = write {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error);
    }
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.message);
    }
    Json(json!({"ok":true})).into_response()
}

enum TogglePackage {
    Missing,
    Kept,
    Changed,
    Unchanged,
}

fn toggle_package(entries: &mut Value, source: &str, disable: bool, keep_filters: bool) -> TogglePackage {
    let Some(entries) = entries.as_array_mut() else { return TogglePackage::Missing };
    let Some(entry) = entries.iter_mut().find(|entry| package_source(entry) == Some(source)) else {
        return TogglePackage::Missing;
    };
    let disabled = is_disabled(entry);
    if disabled == disable {
        return TogglePackage::Unchanged;
    }
    if disable && keep_filters && entry.is_object() {
        return TogglePackage::Kept;
    }
    if disable {
        if entry.is_string() {
            *entry = json!({"source":source,"extensions":[],"skills":[],"prompts":[],"themes":[]});
        } else if let Some(object) = entry.as_object_mut() {
            for field in ["extensions", "skills", "prompts", "themes"] {
                object.insert(field.to_owned(), json!([]));
            }
        }
    } else if let Some(object) = entry.as_object_mut() {
        for field in ["extensions", "skills", "prompts", "themes"] {
            object.remove(field);
        }
        if object.len() == 1 {
            *entry = Value::String(source.to_owned());
        }
    }
    TogglePackage::Changed
}

fn package_source(entry: &Value) -> Option<&str> {
    entry.as_str().or_else(|| entry.get("source").and_then(Value::as_str))
}

fn is_disabled(entry: &Value) -> bool {
    entry.is_object()
        && ["extensions", "skills", "prompts", "themes"]
            .iter()
            .all(|field| entry.get(*field).and_then(Value::as_array).is_some_and(Vec::is_empty))
}

fn native_extension_installed(agent_dir: &Path, executable: &str) -> bool {
    let Ok(target) = PathBuf::from(executable).canonicalize() else { return false };
    let root = agent_dir.join("extensions");
    let Ok(entries) = fs::read_dir(root) else { return false };
    entries.flatten().any(|entry| {
        let manifest_path = entry.path().join("package.json");
        let Ok(manifest) = fs::read(manifest_path)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(std::io::Error::other))
        else {
            return false;
        };
        manifest.pointer("/pi/native").and_then(Value::as_array).is_some_and(|commands| {
            commands.iter().any(|command| {
                let Some(command) = command.get("command").and_then(Value::as_str) else { return false };
                let path = Path::new(command);
                let candidate = if path.is_absolute() || command.starts_with("\\\\") {
                    path.to_path_buf()
                } else {
                    entry.path().join(path)
                };
                candidate.canonicalize().is_ok_and(|path| path == target)
            })
        })
    })
}
