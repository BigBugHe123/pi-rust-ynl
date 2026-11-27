use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::packages::{PackageManager, PackageManagerOptions};
use pi_coding_agent::resources::{ResourceLoader, ResourceLoaderOptions, ResourceSettings};
use pi_coding_agent::settings::{ResourceKind, SettingsManager, SettingsOptions, SettingsScope};
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;
use tokio::time::timeout;

use crate::project_trust::is_trusted_for_resources;

fn resource_settings(settings: &SettingsManager, scope: SettingsScope) -> ResourceSettings {
    ResourceSettings {
        extensions: settings.resource_paths(ResourceKind::Extensions, scope),
        skills: settings.resource_paths(ResourceKind::Skills, scope),
        prompts: settings.resource_paths(ResourceKind::Prompts, scope),
        themes: settings.resource_paths(ResourceKind::Themes, scope),
    }
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

    // Resolve package resources through pi-rust's package manager, then use its
    // native ResourceLoader with extension loading disabled (the listing never
    // ran extension startup hooks on the Pi Web side).
    let package_manager = PackageManager::new(PackageManagerOptions::new(cwd, agent_dir, settings.clone()));
    let package_resources = if settings.packages(SettingsScope::Global).is_empty()
        && settings.packages(SettingsScope::Project).is_empty()
    {
        Default::default()
    } else {
        match package_manager.resolve_package_resources(None).await {
            Ok(resources) => resources,
            Err(_) => Default::default(),
        }
    };
    let mut options = ResourceLoaderOptions::new(cwd, agent_dir);
    options.project_trusted = trusted;
    options.user_settings = resource_settings(&settings, SettingsScope::Global);
    options.project_settings = resource_settings(&settings, SettingsScope::Project);
    options.package_resources = package_resources;
    options.no_extensions = true;
    let mut loader = ResourceLoader::new(options);
    loader.reload();
    let loaded = loader.resources();
    let mut skills = serde_json::to_value(&loaded.skills).unwrap_or_default();
    annotate_installs(&mut skills, cwd, agent_dir);
    let diagnostics = serde_json::to_value(&loaded.skill_diagnostics).unwrap_or_default();
    Json(json!({
        "skills": skills,
        "diagnostics": diagnostics,
        "projectResourcesLoaded": trusted,
    }))
    .into_response()
}

fn annotate_installs(skills: &mut serde_json::Value, cwd: &Path, agent_dir: &Path) {
    let home = pi_coding_agent::resources::paths::home_dir();
    let global_lock = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".agents"))
        .join("skills")
        .join(".skill-lock.json");
    // XDG_STATE_HOME already names the state root; the default is ~/.agents.
    let global_lock = if std::env::var_os("XDG_STATE_HOME").is_some() {
        global_lock
    } else {
        home.join(".agents/skills/.skill-lock.json")
    };
    let project_lock = cwd.join("skills-lock.json");
    let read = |path: &Path| -> serde_json::Value {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .and_then(|v: serde_json::Value| v.get("skills").cloned())
            .unwrap_or_else(|| json!({}))
    };
    let global = read(&global_lock);
    let project = read(&project_lock);
    let home_skills = home.join(".agents/skills");
    let global_roots = [agent_dir.join("skills"), home_skills];
    let project_roots = [cwd.join(".pi/skills"), cwd.join(".agents/skills")];
    let Some(items) = skills.as_array_mut() else { return };
    for skill in items {
        let Some(file) = skill.get("filePath").and_then(serde_json::Value::as_str).map(PathBuf::from) else { continue };
        let name = skill.get("name").and_then(serde_json::Value::as_str).unwrap_or("");
        let (entries, scope) = if global_roots.iter().any(|root| file.starts_with(root)) {
            (&global, "global")
        } else if project_roots.iter().any(|root| file.starts_with(root)) {
            (&project, "project")
        } else {
            continue;
        };
        let entry = entries.get(name).or_else(|| {
            entries.as_object().and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
        });
        let Some(entry) = entry else { continue };
        let Some(source) = entry.get("source").and_then(serde_json::Value::as_str).filter(|s| !s.trim().is_empty())
        else {
            continue;
        };
        let source_type = entry.get("sourceType").and_then(serde_json::Value::as_str);
        let source = if source_type == Some("github") {
            source
                .trim()
                .trim_start_matches("git+")
                .trim_start_matches("https://github.com/")
                .trim_start_matches("http://github.com/")
                .trim_start_matches("git@github.com:")
                .trim_end_matches(".git")
                .trim_end_matches('/')
        } else {
            source.trim().trim_end_matches('/')
        };
        let skill_path = entry.get("skillPath").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty());
        let ref_name = entry.get("ref").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty());
        let hash_key = if scope == "global" { "skillFolderHash" } else { "computedHash" };
        let has_hash = entry.get(hash_key).and_then(serde_json::Value::as_str).is_some_and(|hash| !hash.is_empty());
        let valid_github_source = source.split_once('/').is_some_and(|(owner, repo)| {
            !owner.is_empty()
                && !repo.is_empty()
                && repo.contains('/') == false
                && source.chars().all(|ch| ch.is_ascii_alphanumeric() || "_.-/".contains(ch))
        });
        let can_check = source_type == Some("github")
            && valid_github_source
            && skill_path.is_some()
            && has_hash
            && (scope == "global" || ref_name.is_none());
        let mut install = json!({
            "package": format!("{source}@{name}"), "scope": scope, "source": source,
            "canCheckForUpdates": can_check
        });
        if let Some(ty) = source_type {
            install["sourceType"] = json!(ty);
        }
        if source_type != Some("local") && !source.is_empty() && !source.contains("://") && !source.starts_with("git@")
        {
            let url_source = source.split('/').map(encode_component).collect::<Vec<_>>().join("/");
            install["skillsShUrl"] = json!(format!("https://skills.sh/{url_source}/{}", encode_component(name)));
        }
        for key in ["skillPath", "ref"] {
            if let Some(v) = entry.get(key) {
                install[key] = v.clone();
            }
        }
        if let Some(v) = entry.get(hash_key) {
            install["versionHash"] = v.clone();
        }
        skill["install"] = install;
    }
}

#[derive(Deserialize)]
pub struct ContentQuery {
    pub cwd: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub document: Option<String>,
}

pub async fn content(agent_dir: &Path, query: ContentQuery) -> Response {
    let cwd = resolve_cwd(&query.cwd);
    if !cwd.is_dir() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"Directory does not exist"}))).into_response();
    }
    let trusted = is_trusted_for_resources(&cwd, agent_dir);
    let settings = Arc::new(SettingsManager::create(&cwd, agent_dir, SettingsOptions { project_trusted: trusted }));
    let pm = PackageManager::new(PackageManagerOptions::new(&cwd, agent_dir, settings.clone()));
    let package_resources = if settings.packages(SettingsScope::Global).is_empty()
        && settings.packages(SettingsScope::Project).is_empty()
    {
        Default::default()
    } else {
        pm.resolve_package_resources(None).await.unwrap_or_default()
    };
    let mut options = ResourceLoaderOptions::new(&cwd, agent_dir);
    options.project_trusted = trusted;
    options.user_settings = resource_settings(&settings, SettingsScope::Global);
    options.project_settings = resource_settings(&settings, SettingsScope::Project);
    options.package_resources = package_resources;
    options.no_extensions = true;
    let mut loader = ResourceLoader::new(options);
    loader.reload();
    let file = PathBuf::from(&query.file_path);
    let skill = loader.resources().skills.iter().find(|skill| PathBuf::from(&skill.file_path) == file);
    let Some(skill) = skill else {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"Skill not found"}))).into_response();
    };
    let document_requested = query.document.is_some();
    let base_dir = PathBuf::from(&skill.base_dir);
    let target = if let Some(document) = query.document {
        if Path::new(&document).is_absolute()
            || document.contains(['\\', ':', '\0'])
            || document.split('/').any(|s| s == "..")
        {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":"Document path must stay inside the skill folder"})))
                .into_response();
        }
        if !document.to_ascii_lowercase().ends_with(".md") && !document.to_ascii_lowercase().ends_with(".markdown") {
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                Json(json!({"error":"Only Markdown documents can be opened"})),
            )
                .into_response();
        }
        let target = base_dir.join(document);
        if !target.exists() {
            return (StatusCode::NOT_FOUND, Json(json!({"error":"Skill document not found"}))).into_response();
        }
        let canonical = match target.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                return (StatusCode::FORBIDDEN, Json(json!({"error":"Document is outside the skill folder"})))
                    .into_response();
            }
        };
        let base_canonical = base_dir.canonicalize().unwrap_or(base_dir.clone());
        if !canonical.starts_with(base_canonical) {
            return (StatusCode::FORBIDDEN, Json(json!({"error":"Document is outside the skill folder"})))
                .into_response();
        }
        if !canonical
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
        {
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                Json(json!({"error":"Only Markdown documents can be opened"})),
            )
                .into_response();
        }
        canonical
    } else {
        let canonical = match file.canonicalize() {
            Ok(path) => path,
            Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error":"Skill file not found"}))).into_response(),
        };
        let canonical_base = base_dir.canonicalize().unwrap_or(base_dir);
        if !canonical.starts_with(canonical_base) {
            return (StatusCode::FORBIDDEN, Json(json!({"error":"Access denied"}))).into_response();
        }
        canonical
    };
    match std::fs::metadata(&target) {
        Ok(meta) if !meta.is_file() => {
            (StatusCode::BAD_REQUEST, Json(json!({"error":"Skill document is not a regular file"}))).into_response()
        }
        Ok(meta) if meta.len() > 1024 * 1024 => {
            (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error":"Skill document is larger than 1 MiB"})))
                .into_response()
        }
        Ok(_) => match std::fs::read_to_string(&target) {
            Ok(content) => Json(json!({"content":content})).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"Skill document could not be read"})))
                .into_response(),
        },
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": if document_requested { "Skill document not found" } else { "Skill file not found" }}),
            ),
        )
            .into_response(),
    }
}

fn cli_command(args: &[String], cwd: Option<&Path>) -> Command {
    #[cfg(windows)]
    let mut command = {
        // Package arguments are validated before this is called; reject shell metacharacters
        // so cmd.exe cannot reinterpret them while launching npm's npx.cmd shim.
        let joined = args.iter().map(|arg| format!("\"{arg}\"")).collect::<Vec<_>>().join(" ");
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/D", "/S", "/C", &format!("npx {joined}")]);
        cmd
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut cmd = Command::new("npx");
        cmd.args(args);
        cmd
    };
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.env("FORCE_COLOR", "0").env_remove("PI_WEB_PASSWORD").env_remove("PORT").env_remove("NODE_ENV");
    let next_vars = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.to_ascii_uppercase().starts_with("NEXT_"))
        .collect::<Vec<_>>();
    for key in next_vars {
        command.env_remove(key);
    }
    command
}

async fn run_skills_cli(args: Vec<String>, cwd: Option<&Path>) -> Result<(String, String), String> {
    let output = timeout(Duration::from_secs(60), cli_command(&args, cwd).output())
        .await
        .map_err(|_| "Command timed out after 60 seconds".to_owned())?
        .map_err(|error| error.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() { Ok((stdout, stderr)) } else { Err(format!("{stdout}{stderr}")) }
}

pub async fn install(agent_dir: &Path, input: serde_json::Value) -> Response {
    let Some(package) =
        input.get("package").and_then(serde_json::Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"package required"}))).into_response();
    };
    if package.chars().any(|c| !(c.is_ascii_alphanumeric() || "@/._-:#+".contains(c))) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid package"}))).into_response();
    }
    let global = input.get("scope").and_then(serde_json::Value::as_str) != Some("project");
    let cwd = input.get("cwd").and_then(serde_json::Value::as_str).map(resolve_cwd);
    if !global && cwd.is_none() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cwd required for project install"}))).into_response();
    }
    if let Some(cwd) = cwd.as_deref() {
        if !cwd.is_dir() {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":"Directory does not exist"}))).into_response();
        }
        if !global && !is_trusted_for_resources(cwd, agent_dir) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"Project resources must be trusted before installing project skills"})),
            )
                .into_response();
        }
    }
    let mut args = vec![
        "skills".to_owned(),
        "add".to_owned(),
        package.to_owned(),
        "-y".to_owned(),
        "--agent".to_owned(),
        "pi".to_owned(),
    ];
    if global {
        args.push("-g".to_owned());
    }
    match run_skills_cli(args, if global { None } else { cwd.as_deref() }).await {
        Ok((stdout, stderr)) => {
            let output = strip_ansi(&(stdout + &stderr));
            if output.contains("Installation complete") || (output.contains("Installed ") && output.contains(" skill"))
            {
                Json(json!({"success":true,"output":output})).into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":tail(&output, 300)}))).into_response()
            }
        }
        Err(error) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":tail(&strip_ansi(&error), 300)}))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct SkillsSearchResponse {
    skills: Option<Vec<SkillsSearchItem>>,
}

#[derive(Deserialize)]
struct SkillsSearchItem {
    id: Option<String>,
    name: Option<String>,
    source: Option<String>,
    installs: Option<u64>,
}

fn format_installs(count: Option<u64>) -> String {
    let Some(count) = count.filter(|count| *count > 0) else { return String::new() };
    if count >= 1_000_000 {
        let value = count as f64 / 1_000_000.0;
        format!("{}M installs", compact_count(value))
    } else if count >= 1_000 {
        let value = count as f64 / 1_000.0;
        format!("{}K installs", compact_count(value))
    } else {
        format!("{count} install{}", if count == 1 { "" } else { "s" })
    }
}

fn compact_count(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 { format!("{rounded:.0}") } else { format!("{rounded:.1}") }
}

fn install_count(text: &str) -> f64 {
    let Some((number, suffix)) = text.split_once(' ') else { return 0.0 };
    let number = number.trim_end_matches("installs").trim_end_matches("install");
    let (number, multiplier) = match number.chars().last() {
        Some('K') => (&number[..number.len() - 1], 1_000.0),
        Some('M') => (&number[..number.len() - 1], 1_000_000.0),
        Some('B') => (&number[..number.len() - 1], 1_000_000_000.0),
        _ => (number, 1.0),
    };
    if suffix != "installs" && suffix != "install" {
        return 0.0;
    }
    number.parse::<f64>().unwrap_or(0.0) * multiplier
}

fn parse_search_output(raw: &str, limit: usize) -> Vec<serde_json::Value> {
    let clean = strip_ansi(raw);
    let lines = clean.lines().collect::<Vec<_>>();
    let mut results = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let line = line.trim();
        let Some((package, installs)) = line.split_once(char::is_whitespace) else { continue };
        let installs = installs.trim();
        if !package.contains('/') || !installs.ends_with("installs") {
            continue;
        }
        if !package.chars().all(|ch| ch.is_ascii_alphanumeric() || "._-@:/".contains(ch)) {
            continue;
        }
        let url = lines
            .get(index + 1)
            .map(|value| value.trim().trim_start_matches(['│', '|', ' ']))
            .filter(|value| value.starts_with("https://"))
            .unwrap_or("");
        results.push(json!({"package":package,"installs":installs,"url":url}));
        if results.len() >= limit {
            break;
        }
    }
    results
}

pub async fn search(input: serde_json::Value) -> Response {
    let Some(query) =
        input.get("query").and_then(serde_json::Value::as_str).map(str::trim).filter(|query| !query.is_empty())
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"query required"}))).into_response();
    };
    let limit = input
        .get("limit")
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite())
        .map(|value| (value.floor() as usize).clamp(1, 50))
        .unwrap_or(50);
    let base = std::env::var("SKILLS_API_URL").unwrap_or_else(|_| "https://skills.sh".into());
    let api_url = format!(
        "{}/api/search?q={}&limit={limit}",
        base.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let api_result = match reqwest::get(api_url).await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => response.json::<SkillsSearchResponse>().await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    if let Ok(data) = api_result {
        let mut results = data.skills.unwrap_or_default().into_iter().filter_map(|skill| {
            let name = skill.name?.trim().to_owned();
            if name.is_empty() { return None; }
            let slug = skill.id.unwrap_or_default().trim().to_owned();
            let source = skill.source.unwrap_or_default().trim().to_owned();
            let package_source = if source.is_empty() { &slug } else { &source };
            if package_source.is_empty() { return None; }
            Some(json!({"package":format!("{package_source}@{name}"),"installs":format_installs(skill.installs),"url":if slug.is_empty() { String::new() } else { format!("{}/{}", base.trim_end_matches('/'), slug) }}))
        }).collect::<Vec<_>>();
        results.sort_by(|left, right| {
            install_count(right["installs"].as_str().unwrap_or(""))
                .partial_cmp(&install_count(left["installs"].as_str().unwrap_or("")))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        return Json(json!({"results":results})).into_response();
    }

    let output = timeout(
        Duration::from_secs(20),
        cli_command(&["skills".into(), "find".into(), query.to_owned()], None).output(),
    )
    .await;
    match output {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!("{stdout}{stderr}");
            let results = parse_search_output(&combined, limit);
            if output.status.success() || !results.is_empty() {
                Json(json!({"results":results})).into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":tail(&strip_ansi(&combined),300)})))
                    .into_response()
            }
        }
        Ok(Err(error)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error.to_string()}))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"Command timed out after 20 seconds"})))
            .into_response(),
    }
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(c) = chars.next() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}
fn tail(text: &str, max: usize) -> String {
    let start = text.char_indices().rev().nth(max).map(|(i, _)| i).unwrap_or(0);
    text[start..].to_owned()
}
fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallInfo {
    package: String,
    scope: String,
    source: String,
    #[serde(default)]
    source_type: Option<String>,
    #[serde(default)]
    skill_path: Option<String>,
    #[serde(default, rename = "ref")]
    ref_name: Option<String>,
    #[serde(default)]
    version_hash: Option<String>,
    #[serde(default)]
    can_check_for_updates: bool,
    #[serde(default)]
    skills_sh_url: Option<String>,
}

pub async fn check_updates(input: serde_json::Value) -> Response {
    let Some(installs) = input.get("installs").and_then(serde_json::Value::as_array) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"installs required"}))).into_response();
    };
    let mut results = Vec::with_capacity(installs.len());
    for value in installs {
        let Ok(install) = serde_json::from_value::<InstallInfo>(value.clone()) else { continue };
        results.push(check_one(&install).await);
    }
    Json(json!({"updates":results})).into_response()
}

async fn check_one(install: &InstallInfo) -> serde_json::Value {
    let current = install.version_hash.clone();
    let result = |state: &str, latest: Option<String>, message: Option<String>| json!({"package":install.package,"scope":install.scope,"state":state,"currentVersion":current,"latestVersion":latest,"message":message});
    if !install.can_check_for_updates || current.is_none() || install.skill_path.is_none() {
        return result("unsupported", None, Some("This lock entry cannot be checked automatically.".into()));
    }
    let path = install.skill_path.as_deref().unwrap_or("").replace('\\', "/");
    let folder = path
        .strip_suffix("/SKILL.md")
        .or_else(|| path.strip_suffix("/skill.md"))
        .unwrap_or(&path)
        .trim_end_matches('/');
    let latest: Result<Option<String>, String> = if install.scope == "global" {
        let reference = install.ref_name.as_deref().unwrap_or("HEAD");
        let url = format!(
            "https://api.github.com/repos/{}/git/trees/{}?recursive=1",
            install.source,
            encode_component(reference)
        );
        let mut request = reqwest::Client::new()
            .get(url)
            .header("User-Agent", "pi-web")
            .header("Accept", "application/vnd.github.v3+json")
            .timeout(Duration::from_secs(15));
        if let Some(token) = std::env::var("GITHUB_TOKEN").ok().or_else(|| std::env::var("GH_TOKEN").ok()) {
            request = request.bearer_auth(token);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => match response.json::<serde_json::Value>().await {
                Ok(json) if folder.is_empty() => {
                    Ok(json.get("sha").and_then(serde_json::Value::as_str).map(str::to_owned))
                }
                Ok(json) => Ok(json
                    .get("tree")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|entries| {
                        entries.iter().find(|entry| {
                            entry.get("path").and_then(serde_json::Value::as_str) == Some(folder)
                                && entry.get("type").and_then(serde_json::Value::as_str) == Some("tree")
                        })
                    })
                    .and_then(|entry| entry.get("sha"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)),
                Err(error) => Err(error.to_string()),
            },
            Ok(response) => Err(format!(
                "HTTP {}{}",
                response.status(),
                if [401, 403, 429].contains(&response.status().as_u16()) {
                    " (git fallback unavailable in pi-web-backend)"
                } else {
                    ""
                }
            )),
            Err(error) => Err(error.to_string()),
        }
    } else {
        let mut parts = install.source.split('/');
        let owner = parts.next().unwrap_or("");
        let repo = parts.next().unwrap_or("");
        let name = install
            .package
            .rsplit('@')
            .next()
            .unwrap_or(&install.package)
            .to_ascii_lowercase()
            .replace([' ', '_'], "-");
        let mut slug = String::new();
        let mut previous_hyphen = false;
        for ch in name.chars() {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                slug.push(ch);
                previous_hyphen = false;
            } else if ch == '-' && !previous_hyphen {
                slug.push(ch);
                previous_hyphen = true;
            }
        }
        let slug = slug.trim_matches('-');
        let base = std::env::var("SKILLS_API_URL").unwrap_or_else(|_| "https://skills.sh".into());
        let url = format!(
            "{}/api/download/{}/{}/{}",
            base.trim_end_matches('/'),
            encode_component(owner),
            encode_component(repo),
            encode_component(&slug)
        );
        match reqwest::Client::new().get(url).timeout(Duration::from_secs(15)).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    match response
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                        .and_then(|v| v.get("hash").and_then(serde_json::Value::as_str).map(str::to_owned))
                    {
                        Some(hash) => Ok(Some(hash)),
                        None => Err("skills.sh did not return a version hash.".into()),
                    }
                } else {
                    Err(format!("HTTP {}", response.status()))
                }
            }
            Err(error) => Err(error.to_string()),
        }
    };
    match latest {
        Ok(Some(hash)) => {
            result(if Some(&hash) == current.as_ref() { "up-to-date" } else { "update-available" }, Some(hash), None)
        }
        Ok(None) => result("error", None, Some("Remote skill path was not found.".into())),
        Err(message) => result("error", None, Some(message)),
    }
}

pub async fn update(agent_dir: &Path, input: serde_json::Value) -> Response {
    let Some(mut install) = input.get("install").cloned().and_then(|v| serde_json::from_value::<InstallInfo>(v).ok())
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"install required"}))).into_response();
    };
    if !install.can_check_for_updates {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"This skill cannot be updated automatically"})))
            .into_response();
    }
    let cwd = input.get("cwd").and_then(serde_json::Value::as_str).map(resolve_cwd);
    if install.scope == "project" && cwd.is_none() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cwd required"}))).into_response();
    }
    if let Some(cwd) = cwd.as_deref() {
        if !is_trusted_for_resources(cwd, agent_dir) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"Project resources must be trusted before updating project skills"})),
            )
                .into_response();
        }
    }
    let folder = install.skill_path.as_deref().unwrap_or("").replace('\\', "/");
    let folder = folder
        .strip_suffix("/SKILL.md")
        .or_else(|| folder.strip_suffix("/skill.md"))
        .unwrap_or(&folder)
        .trim_end_matches('/');
    let source = if folder.is_empty() { install.source.clone() } else { format!("{}/{folder}", install.source) };
    let mut package_source = source;
    if let Some(reference) = install.ref_name.as_deref() {
        package_source.push('#');
        package_source.push_str(&encode_component(reference));
    }
    if package_source.chars().any(|c| !(c.is_ascii_alphanumeric() || "@/._-:#+".contains(c))) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid skill source"}))).into_response();
    }
    let skill_name = install.package.rsplit('@').next().unwrap_or(&install.package).to_owned();
    let mut args = vec![
        "skills".into(),
        "add".into(),
        package_source,
        "--skill".into(),
        skill_name,
        "-y".into(),
        "--agent".into(),
        "pi".into(),
    ];
    if install.scope == "global" {
        args.push("-g".into());
    }
    match run_skills_cli(args, if install.scope == "project" { cwd.as_deref() } else { None }).await {
        Ok((stdout, stderr)) => {
            install.version_hash = None;
            Json(json!({"success":true,"output":tail(&(stdout+&stderr),500)})).into_response()
        }
        Err(error) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":tail(&strip_ansi(&error),500)}))).into_response()
        }
    }
}

pub fn toggle(input: serde_json::Value) -> Response {
    let Some(disable) = input.get("disableModelInvocation").and_then(serde_json::Value::as_bool) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"disableModelInvocation must be a boolean"})))
            .into_response();
    };
    let roots = input
        .get("allowedRoots")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let paths = if let Some(paths) = input.get("filePaths").and_then(serde_json::Value::as_array) {
        let Some(paths) = paths.iter().map(serde_json::Value::as_str).collect::<Option<Vec<_>>>() else {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":"filePaths must be a list of paths"})))
                .into_response();
        };
        paths
    } else if let Some(path) = input.get("filePath").and_then(serde_json::Value::as_str) {
        vec![path]
    } else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"filePath required"}))).into_response();
    };
    let batch = input.get("filePaths").is_some();
    let mut seen = std::collections::HashSet::new();
    let results = paths
        .into_iter()
        .filter(|path| seen.insert(*path))
        .map(|raw| {
            let path = PathBuf::from(raw);
            let error =
                if path.extension().and_then(|ext| ext.to_str()).is_none_or(|ext| !ext.eq_ignore_ascii_case("md")) {
                    Some("Not a skill file".to_owned())
                } else {
                    match path.canonicalize() {
                        Err(_) => Some("file not found".to_owned()),
                        Ok(target) if !target.is_file() => Some("Not a regular file".to_owned()),
                        Ok(target)
                            if !roots.iter().any(|root| {
                                target.starts_with(root.canonicalize().unwrap_or_else(|_| root.clone()))
                            }) =>
                        {
                            Some("Access denied".to_owned())
                        }
                        Ok(target) => match std::fs::read_to_string(&target) {
                            Err(error) => Some(error.to_string()),
                            Ok(content) => match set_disable_model_invocation(&content, disable) {
                                Err(error) => Some(error),
                                Ok(updated) if updated == content => None,
                                Ok(updated) => std::fs::write(target, updated).err().map(|error| error.to_string()),
                            },
                        },
                    }
                };
            error.map_or_else(|| json!({"filePath":raw}), |error| json!({"filePath":raw,"error":error}))
        })
        .collect::<Vec<_>>();
    if batch {
        Json(json!({"results":results})).into_response()
    } else if let Some(error) = results.first().and_then(|result| result.get("error")) {
        let status = match error.as_str() {
            Some("file not found") => StatusCode::NOT_FOUND,
            Some("Access denied") => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_REQUEST,
        };
        (status, Json(json!({"error":error}))).into_response()
    } else {
        Json(json!({"success":true})).into_response()
    }
}

fn set_disable_model_invocation(content: &str, disable: bool) -> Result<String, String> {
    const KEY: &str = "disable-model-invocation";
    let (bom, body) = content.strip_prefix('\u{feff}').map_or((false, content), |body| (true, body));
    let newline = if body.contains("\r\n") {
        "\r\n"
    } else if body.contains('\r') {
        "\r"
    } else {
        "\n"
    };
    if !body.starts_with("---") {
        return if disable {
            Ok(format!("{}---{newline}{KEY}: true{newline}---{newline}{body}", if bom { "\u{feff}" } else { "" }))
        } else {
            Ok(content.to_owned())
        };
    }
    let opening_end = body
        .find(['\r', '\n'])
        .map(|index| index + if body[index..].starts_with("\r\n") { 2 } else { 1 })
        .ok_or_else(|| format!("Cannot edit {KEY}: unsupported frontmatter formatting"))?;
    let tail = &body[opening_end..];
    let mut offset = opening_end;
    let mut closing = None;
    for line in split_lines(tail) {
        if line.trim_start().starts_with("---") {
            closing = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let Some((closing_start, closing_end)) = closing else {
        return Err(format!("Cannot edit {KEY}: unsupported frontmatter formatting"));
    };
    let head = &body[opening_end..closing_start];
    if disable && head.trim_start().starts_with('{') && head.contains(KEY) {
        return Err(format!("Cannot edit {KEY}: unsupported frontmatter formatting"));
    }
    let mut lines = split_lines(head).into_iter().map(str::to_owned).collect::<Vec<_>>();
    let root_indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .map(|line| &line[..line.len() - line.trim_start().len()])
        .min_by_key(|indent| indent.len())
        .unwrap_or("")
        .to_owned();
    let key_line = lines.iter().position(|line| {
        let trimmed = line.trim_start();
        line.len() - trimmed.len() == root_indent.len()
            && [KEY.to_owned(), format!("\"{KEY}\""), format!("'{KEY}'")]
                .iter()
                .any(|key| trimmed.starts_with(key) && trimmed[key.len()..].trim_start().starts_with(':'))
    });
    if !disable && key_line.is_none() {
        return Ok(content.to_owned());
    }
    if disable {
        if let Some(index) = key_line {
            let line = &lines[index];
            let indent = &line[..line.len() - line.trim_start().len()];
            let ending = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            lines[index] = format!("{indent}{KEY}: true{ending}");
        } else {
            lines.insert(0, format!("{root_indent}{KEY}: true{newline}"));
        }
    } else if let Some(index) = key_line {
        lines.remove(index);
    }
    let mut updated = String::with_capacity(content.len() + 32);
    if bom {
        updated.push('\u{feff}');
    }
    updated.push_str(&body[..opening_end]);
    for line in lines {
        updated.push_str(&line);
    }
    updated.push_str(&body[closing_start..closing_end]);
    updated.push_str(&body[closing_end..]);
    Ok(updated)
}

fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' || bytes[index] == b'\r' {
            let end = if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') { index + 2 } else { index + 1 };
            lines.push(&text[start..end]);
            start = end;
            index = end;
        } else {
            index += 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

pub fn resolve_cwd(value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() { path } else { std::env::current_dir().unwrap_or_default().join(path) }
}
