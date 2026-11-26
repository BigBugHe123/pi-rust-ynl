use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::future::BoxFuture;
use pi_coding_agent::packages::{MissingSourceAction, MissingSourceHandler, PackageManager, PackageManagerOptions};
use pi_coding_agent::resources::{ResolvedPaths, ResolvedResource, SourceOrigin, SourceScope};
use pi_coding_agent::settings::schema::PackageSource;
use pi_coding_agent::settings::{ResourceKind, SettingsManager, SettingsOptions, SettingsScope};
use serde_json::{Value, json};

use crate::web_backend::project_trust::is_trusted_for_resources;

fn scope_name(scope: SourceScope) -> &'static str {
    match scope {
        SourceScope::Project => "project",
        SourceScope::User | SourceScope::Temporary => "global",
    }
}

fn disabled(entry: &PackageSource) -> bool {
    let PackageSource::Filtered(filter) = entry else { return false };
    [&filter.extensions, &filter.skills, &filter.prompts, &filter.themes]
        .iter()
        .all(|list| list.as_ref().is_some_and(|entries| entries.is_empty()))
}

fn package_source(entry: &PackageSource) -> &str {
    match entry {
        PackageSource::Source(source) => source,
        PackageSource::Filtered(filter) => &filter.source,
    }
}

fn empty_counts() -> Value {
    json!({"extensions":0,"skills":0,"prompts":0,"themes":0})
}

fn add_resource(
    resource: &ResolvedResource,
    kind: &str,
    counts: &mut HashMap<String, Value>,
    resources: &mut HashMap<String, Vec<Value>>,
    totals: &mut HashMap<String, usize>,
) {
    if !resource.enabled || resource.metadata.origin != SourceOrigin::Package {
        return;
    }
    let scope = scope_name(resource.metadata.scope);
    let key = format!("{scope}\0{}", resource.metadata.source);
    let count = counts.entry(key.clone()).or_insert_with(empty_counts);
    count[kind] = json!(count[kind].as_u64().unwrap_or_default() + 1);
    *totals.entry(kind.to_owned()).or_default() += 1;
    let path = &resource.path;
    let file_name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default();
    let stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or(file_name);
    let name = if kind == "skills" && file_name.eq_ignore_ascii_case("SKILL.md") {
        path.parent().and_then(Path::file_name).and_then(|value| value.to_str()).unwrap_or(stem)
    } else if kind == "extensions" && matches!(file_name, "index.ts" | "index.js") {
        path.parent().and_then(Path::file_name).and_then(|value| value.to_str()).unwrap_or(stem)
    } else {
        stem
    };
    let relative = resource
        .metadata
        .base_dir
        .as_ref()
        .and_then(|base| path.strip_prefix(base).ok())
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| path.display().to_string());
    resources.entry(key).or_default().push(json!({
        "kind": kind.trim_end_matches('s'),
        "name": name,
        "path": path,
        "relativePath": relative,
    }));
}

fn configured_version(source: &str) -> Option<String> {
    if let Some(npm) = source.strip_prefix("npm:") {
        let last_at = npm.rfind('@')?;
        let package_end = if npm.starts_with('@') { npm.find('/')? } else { 0 };
        return (last_at > package_end).then(|| npm[last_at + 1..].to_owned()).filter(|value| !value.is_empty());
    }
    if source.starts_with("git:") || source.contains("://") {
        let last_at = source.rfind('@')?;
        let last_sep = source.rfind('/').unwrap_or_default().max(source.rfind(':').unwrap_or_default());
        return (last_at > last_sep).then(|| source[last_at + 1..].to_owned()).filter(|value| !value.is_empty());
    }
    None
}

fn checkable(source: &str) -> bool {
    if let Some(npm) = source.strip_prefix("npm:") {
        let last_at = npm.rfind('@');
        let package_end = if npm.starts_with('@') { npm.find('/').unwrap_or(0) } else { 0 };
        return last_at.is_none_or(|index| index <= package_end);
    }
    if source.starts_with("git:")
        || ["http://", "https://", "ssh://", "git://"].iter().any(|prefix| source.starts_with(prefix))
    {
        return configured_version(source).is_none();
    }
    false
}

fn metadata(path: Option<&Path>) -> (Option<String>, Option<String>, Option<String>) {
    let Some(path) = path else { return (None, None, None) };
    let package_json =
        if path.is_dir() { path.join("package.json") } else { path.parent().unwrap_or(path).join("package.json") };
    let Ok(bytes) = fs::read(package_json) else { return (None, None, None) };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { return (None, None, None) };
    (
        value.get("name").and_then(Value::as_str).map(str::to_owned),
        value.get("version").and_then(Value::as_str).map(str::to_owned),
        value.get("description").and_then(Value::as_str).map(str::to_owned),
    )
}

fn native_enabled(path: &str, entries: &[String]) -> bool {
    let target = path.replace('\\', "/");
    let mut enabled = true;
    for entry in entries {
        let exact = entry.starts_with(['+', '-']);
        let pattern = entry.trim_start_matches(['+', '-', '!']).replace('\\', "/");
        if if exact { pattern == target } else { pattern == target || target.ends_with(&format!("/{pattern}")) } {
            enabled = !entry.starts_with(['-', '!']);
        }
    }
    enabled
}

fn native_extensions(agent_dir: &Path, settings: &SettingsManager) -> Vec<Value> {
    let dir = agent_dir.join("extensions");
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let settings_paths = settings.resource_paths(ResourceKind::Extensions, SettingsScope::Global);
    let mut out = Vec::new();
    for entry in entries.flatten().filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir())) {
        let root = entry.path();
        let manifest_path = root.join("package.json");
        let Some(manifest) = pi_coding_agent::packages::manifest::read_package_json(&manifest_path) else { continue };
        let Some(commands) = manifest.pointer("/pi/native").and_then(Value::as_array) else { continue };
        let entry_file_name = entry.file_name();
        let package_name = manifest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| entry_file_name.to_str().unwrap_or_default());
        let suffix = package_name.rsplit('/').next().unwrap_or(package_name);
        for (index, command) in commands.iter().enumerate() {
            let Some(command) = command.get("command").and_then(Value::as_str) else { continue };
            let command_path = Path::new(command);
            let mut executable = if command_path.is_absolute() || command.starts_with("\\\\") {
                command_path.to_path_buf()
            } else {
                root.join(command_path)
            };
            if cfg!(windows) && executable.extension().is_none() {
                executable.set_extension("exe");
            }
            if !executable.exists() {
                continue;
            }
            let executable_text = executable.display().to_string();
            let name = if commands.len() == 1 { suffix.to_owned() } else { format!("{suffix} ({})", index + 1) };
            let mut value = json!({
                "kind":"extension",
                "name":name,
                "path":executable_text,
                "relativePath":entry.file_name(),
                "scope":"global",
                "enabled":native_enabled(&executable_text, &settings_paths),
            });
            if let Some(description) = manifest.get("description").and_then(Value::as_str) {
                value["description"] = json!(description);
            }
            out.push(value);
        }
    }
    out
}

pub async fn list(agent_dir: &Path, cwd: &Path) -> Response {
    if !cwd.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("Directory does not exist: {}", cwd.display())})),
        )
            .into_response();
    }
    let trusted = is_trusted_for_resources(cwd, agent_dir);
    let settings = Arc::new(SettingsManager::create(cwd, agent_dir, SettingsOptions { project_trusted: trusted }));
    let mut options = PackageManagerOptions::new(cwd, agent_dir, settings.clone());
    options.builtin_extensions =
        pi_coding_agent::agent_session::BUILTIN_EXTENSIONS.iter().map(|name| (*name).to_owned()).collect();
    let manager = PackageManager::new(options);
    let diagnostics = Arc::new(Mutex::new(Vec::<Value>::new()));
    let diagnostic_sink = diagnostics.clone();
    let missing: MissingSourceHandler = Arc::new(move |source| {
        let sink = diagnostic_sink.clone();
        Box::pin(async move {
            sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(json!({
                "type":"warning",
                "source":source,
                "message":"Package is configured but not installed yet.",
            }));
            MissingSourceAction::Skip
        }) as BoxFuture<'static, MissingSourceAction>
    });
    let resolved = match manager.resolve(Some(&missing)).await {
        Ok(paths) => paths,
        Err(error) => {
            diagnostics
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(json!({"type":"error","message":error.to_string()}));
            ResolvedPaths::default()
        }
    };
    for failure in manager.take_install_failures() {
        diagnostics
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(json!({"type":"warning","source":failure.source,"message":failure.warning()}));
    }

    let mut counts = HashMap::<String, Value>::new();
    let mut resources = HashMap::<String, Vec<Value>>::new();
    let mut totals = HashMap::<String, usize>::from([
        ("extensions".into(), 0),
        ("skills".into(), 0),
        ("prompts".into(), 0),
        ("themes".into(), 0),
    ]);
    for item in &resolved.extensions {
        add_resource(item, "extensions", &mut counts, &mut resources, &mut totals);
    }
    for item in &resolved.skills {
        add_resource(item, "skills", &mut counts, &mut resources, &mut totals);
    }
    for item in &resolved.prompts {
        add_resource(item, "prompts", &mut counts, &mut resources, &mut totals);
    }
    for item in &resolved.themes {
        add_resource(item, "themes", &mut counts, &mut resources, &mut totals);
    }

    let mut natives = native_extensions(agent_dir, &settings);
    // Native manifests are returned by extension discovery as `package.json`
    // entries so the runtime can launch their commands. The inventory already
    // renders each native command below with its executable path and proper
    // name; showing the manifest as another extension creates a duplicate row
    // named "package" for every installed native extension.
    let native_manifests = natives
        .iter()
        .filter_map(|native| {
            let executable = Path::new(native["path"].as_str()?);
            fs::canonicalize(executable.parent()?.join("package.json")).ok()
        })
        .collect::<std::collections::HashSet<_>>();
    let mut standalone = resolved.extensions.iter().filter(|item| {
        item.metadata.origin == SourceOrigin::TopLevel
            && !(item.path.file_name().is_some_and(|name| name == "package.json")
                && fs::canonicalize(&item.path).is_ok_and(|path| native_manifests.contains(&path)))
    }).map(|item| {
        let path = &item.path;
        let file = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let name = if matches!(file, "index.ts" | "index.js") { path.parent().and_then(Path::file_name).and_then(|name| name.to_str()).unwrap_or(file) } else { path.file_stem().and_then(|name| name.to_str()).unwrap_or(file) };
        let relative = item.metadata.base_dir.as_ref().and_then(|base| path.strip_prefix(base).ok()).map(|path| path.to_string_lossy().replace('\\', "/")).filter(|path| !path.is_empty()).unwrap_or_else(|| path.display().to_string());
        json!({"kind":"extension","name":name,"path":path,"relativePath":relative,"scope":scope_name(item.metadata.scope),"enabled":item.enabled})
    }).collect::<Vec<_>>();
    *totals.entry("extensions".to_owned()).or_default() += natives.len();
    standalone.append(&mut natives);

    let disabled_packages = [SettingsScope::Global, SettingsScope::Project]
        .into_iter()
        .flat_map(|scope| {
            settings.packages(scope).into_iter().filter(|package| disabled(package)).map(move |package| {
                (
                    scope_name(if scope == SettingsScope::Project { SourceScope::Project } else { SourceScope::User })
                        .to_owned(),
                    package_source(&package).to_owned(),
                )
            })
        })
        .collect::<std::collections::HashSet<_>>();
    let mut package_rows = Vec::new();
    for package in manager.list_configured_packages().await {
        let scope = scope_name(package.scope);
        let key = format!("{scope}\0{}", package.source);
        let resource_counts = counts.remove(&key).unwrap_or_else(empty_counts);
        let package_resources = resources.remove(&key).unwrap_or_default();
        let resource_count = ["extensions", "skills", "prompts", "themes"]
            .iter()
            .map(|kind| resource_counts[*kind].as_u64().unwrap_or_default())
            .sum::<u64>();
        let is_disabled = disabled_packages.contains(&(scope.to_owned(), package.source.clone()));
        let (package_name, version, description) = metadata(package.installed_path.as_deref());
        if package.installed_path.is_none() {
            diagnostics.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(
                json!({"type":"warning","source":package.source,"message":"Configured package path was not found."}),
            );
        }
        let status = if is_disabled {
            "disabled"
        } else if resource_count > 0 {
            "loaded"
        } else if package.installed_path.is_some() {
            "installed"
        } else {
            "missing"
        };
        let mut row = json!({
            "source":package.source,
            "scope":scope,
            "canCheckForUpdates":checkable(&package.source),
            "filtered":package.filtered,
            "disabled":is_disabled,
            "counts":resource_counts,
            "resources":package_resources,
            "status":status,
        });
        if let Some(path) = package.installed_path {
            row["installedPath"] = json!(path);
        }
        if let Some(name) = package_name {
            row["packageName"] = json!(name);
        }
        if let Some(version) = version {
            row["version"] = json!(version);
        }
        if let Some(version) = configured_version(&package.source) {
            row["configuredVersion"] = json!(version);
        }
        if let Some(description) = description {
            row["description"] = json!(description);
        }
        package_rows.push(row);
    }

    Json(json!({
        "packages":package_rows,
        "standaloneExtensions":standalone,
        "totals":totals,
        "diagnostics":diagnostics.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone(),
        "projectResourcesLoaded":trusted,
    }))
    .into_response()
}
