use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::packages::runner::{CommandRunner, CommandSpec, ProcessRunner};
use pi_coding_agent::packages::source::{ParsedSource, max_version, parse_source, version_gt};
use pi_coding_agent::packages::{PackageManager, PackageManagerOptions};
use pi_coding_agent::resources::SourceScope;
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};
use serde_json::{Value, json};

use crate::web_backend::project_trust::is_trusted_for_resources;

fn scope_name(scope: SourceScope) -> &'static str {
    if scope == SourceScope::Project { "project" } else { "global" }
}

fn offline() -> bool {
    std::env::var("PI_OFFLINE").is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn result(
    source: &str,
    scope: SourceScope,
    display_name: &str,
    kind: &str,
    state: &str,
    message: Option<String>,
) -> Value {
    let mut value = json!({
        "source":source,
        "scope":scope_name(scope),
        "displayName":display_name,
        "type":kind,
        "state":state,
    });
    if let Some(message) = message {
        value["message"] = json!(message);
    }
    value
}

fn package_version(path: &Path) -> anyhow::Result<String> {
    let package_json =
        if path.is_dir() { path.join("package.json") } else { path.parent().unwrap_or(path).join("package.json") };
    let data: Value = serde_json::from_slice(&fs::read(package_json)?)?;
    let version = data
        .get("version")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Installed package version is unavailable."))?;
    Ok(version.to_owned())
}

async fn npm_check(
    runner: &dyn CommandRunner,
    manager: &PackageManager,
    source: &pi_coding_agent::packages::source::NpmSource,
    path: &Path,
    cwd: &Path,
) -> anyhow::Result<bool> {
    let current = package_version(path)?;
    let command = manager.settings().npm_command().unwrap_or_default();
    let (program, prefix_args) =
        command.split_first().map_or(("npm", &[][..]), |(program, args)| (program.as_str(), args));
    if program.is_empty() {
        anyhow::bail!("Invalid npmCommand.");
    }
    let mut args = prefix_args.to_vec();
    args.extend(["view".to_owned(), source.spec.clone(), "version".to_owned(), "--json".to_owned()]);
    let spec = CommandSpec::new(program, args).in_dir(cwd).with_timeout(Duration::from_secs(10));
    let output = runner.capture(&spec).await?;
    let value: Value =
        serde_json::from_str(&output).map_err(|_| anyhow::anyhow!("Unexpected response from npm view."))?;
    let versions = match value {
        Value::String(version) => vec![version],
        Value::Array(values) => values.into_iter().filter_map(|value| value.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    };
    let latest = max_version(&versions, source.version.as_deref().filter(|_| source.has_range))
        .ok_or_else(|| anyhow::anyhow!("Unexpected response from npm view."))?;
    version_gt(&latest, &current)
}

async fn git_capture(runner: &dyn CommandRunner, args: &[&str], cwd: &Path) -> anyhow::Result<String> {
    let spec = CommandSpec::new("git", args.iter().copied())
        .in_dir(cwd)
        .with_env("GIT_TERMINAL_PROMPT", "0")
        .with_timeout(Duration::from_secs(10));
    runner.capture(&spec).await
}

async fn git_check(runner: &dyn CommandRunner, path: &Path) -> anyhow::Result<bool> {
    let local = git_capture(runner, &["rev-parse", "HEAD"], path).await?;
    let upstream = git_capture(runner, &["rev-parse", "--abbrev-ref", "@{upstream}"], path).await.unwrap_or_default();
    let remote_ref = upstream
        .trim()
        .strip_prefix("origin/")
        .filter(|value| !value.is_empty())
        .map(|branch| format!("refs/heads/{branch}"));
    let remote_ref = remote_ref.as_deref().unwrap_or("HEAD");
    let output = git_capture(runner, &["ls-remote", "origin", remote_ref], path).await?;
    let remote = output
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            (hash.len() >= 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(hash)
        })
        .ok_or_else(|| anyhow::anyhow!("Failed to determine remote {remote_ref}."))?;
    Ok(local.trim() != remote)
}

pub async fn check(agent_dir: &Path, input: Value) -> Response {
    let Some(cwd) = input.get("cwd").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"cwd required"}))).into_response();
    };
    let cwd = Path::new(cwd);
    if !cwd.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("Directory does not exist: {}", cwd.display())})),
        )
            .into_response();
    }
    let source_filter = input.get("source").and_then(Value::as_str);
    let scope_filter = input.get("scope").and_then(Value::as_str);
    if source_filter.is_some() != scope_filter.is_some() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"source and scope must be provided together"})))
            .into_response();
    }
    let trusted = is_trusted_for_resources(cwd, agent_dir);
    let settings = Arc::new(SettingsManager::create(cwd, agent_dir, SettingsOptions { project_trusted: trusted }));
    let manager = PackageManager::new(PackageManagerOptions::new(cwd, agent_dir, settings));
    let packages = manager.list_configured_packages().await;
    let selected = packages
        .into_iter()
        .filter(|package| {
            source_filter.is_none_or(|source| package.source == source)
                && scope_filter.is_none_or(|scope| scope_name(package.scope) == scope)
        })
        .collect::<Vec<_>>();
    if source_filter.is_some() && selected.is_empty() {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"Configured package not found"}))).into_response();
    }

    let runner = ProcessRunner::default();
    let mut updates = Vec::with_capacity(selected.len());
    for package in selected {
        let parsed = parse_source(&package.source);
        let (display_name, kind, checkable) = match &parsed {
            ParsedSource::Npm(npm) => (npm.name.as_str(), "npm", !npm.pinned),
            ParsedSource::Git(git) => (package.source.as_str(), "git", !git.pinned),
            _ => (package.source.as_str(), "git", false),
        };
        if !checkable {
            updates.push(result(
                &package.source,
                package.scope,
                display_name,
                kind,
                "unsupported",
                Some("Pinned or local packages cannot be checked automatically.".into()),
            ));
            continue;
        }
        if offline() {
            updates.push(result(
                &package.source,
                package.scope,
                display_name,
                kind,
                "error",
                Some("Update checks are disabled while PI_OFFLINE=1.".into()),
            ));
            continue;
        }
        let Some(path) = package.installed_path.as_deref().filter(|path| path.exists()) else {
            updates.push(result(
                &package.source,
                package.scope,
                display_name,
                kind,
                "error",
                Some("Package is not installed.".into()),
            ));
            continue;
        };
        let checked = match &parsed {
            ParsedSource::Npm(npm) => npm_check(&runner, &manager, npm, path, cwd).await,
            ParsedSource::Git(_) => git_check(&runner, path).await,
            _ => unreachable!(),
        };
        match checked {
            Ok(true) => {
                updates.push(result(&package.source, package.scope, display_name, kind, "update-available", None))
            }
            Ok(false) => updates.push(result(&package.source, package.scope, display_name, kind, "up-to-date", None)),
            Err(error) => updates.push(result(
                &package.source,
                package.scope,
                display_name,
                kind,
                "error",
                Some(error.to_string()),
            )),
        }
    }
    Json(json!({"updates":updates})).into_response()
}
