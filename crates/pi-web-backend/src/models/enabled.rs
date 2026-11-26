//! Enabled model selection API. Pattern resolution is delegated to the same
//! Pi model resolver used by `/api/v1/models`; editing and serialization live
//! here so the web process never reads or rewrites settings.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::enabled_settings;
use crate::web_backend::AppState;
use crate::web_backend::failure;

#[derive(Clone, Debug)]
struct Entry {
    pattern: String,
    matched: Vec<String>,
    pin: Option<String>,
}

#[derive(Clone, Debug)]
struct Input {
    patterns: Option<Vec<String>>,
    available: Vec<String>,
    entries: Vec<Entry>,
    globs: HashMap<String, String>,
}

struct Context {
    cwd: PathBuf,
    scope: String,
    settings_path: String,
    input: Input,
    models: Vec<Value>,
    custom: HashSet<String>,
    model_error: Option<String>,
}

fn within(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn resolve_cwd(state: &AppState, raw: Option<&str>) -> Result<PathBuf, Response> {
    let requested = match raw.filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let canonical = requested
        .canonicalize()
        .map_err(|_| failure(StatusCode::BAD_REQUEST, format!("Directory does not exist: {}", requested.display())))?;
    if !canonical.is_dir() {
        return Err(failure(StatusCode::BAD_REQUEST, format!("Not a directory: {}", canonical.display())));
    }
    if raw.is_some_and(|value| !value.is_empty())
        && !within(&canonical, &crate::web_backend::path_access::allowed_roots(state))
    {
        return Err(failure(StatusCode::FORBIDDEN, "Access denied"));
    }
    Ok(canonical)
}

async fn response_json(response: Response) -> Result<Value, Response> {
    if !response.status().is_success() {
        return Err(response);
    }
    let bytes = to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .map_err(|error| failure(StatusCode::BAD_GATEWAY, format!("Backend response read failed: {error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| failure(StatusCode::BAD_GATEWAY, format!("Invalid backend JSON: {error}")))
}

async fn settings(state: &AppState, cwd: &Path) -> Result<Value, Response> {
    let query = Query(HashMap::from([("cwd".to_string(), cwd.display().to_string())]));
    response_json(enabled_settings::get(State(state.clone()), query).await).await
}

async fn catalog(state: &AppState, cwd: &Path, patterns: &[String]) -> Result<Value, Response> {
    let query = Query(HashMap::from([
        ("cwd".to_string(), cwd.display().to_string()),
        ("scope".to_string(), "all".to_string()),
        ("enabledPatterns".to_string(), serde_json::to_string(patterns).unwrap_or_else(|_| "[]".into())),
        ("withProviderGlobs".to_string(), "1".to_string()),
    ]));
    response_json(crate::web_backend::models(State(state.clone()), query).await).await
}

fn input_from(patterns: Option<Vec<String>>, catalog: &Value) -> Result<Input, Response> {
    let model_list = catalog
        .get("modelList")
        .and_then(Value::as_array)
        .ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "Rust model catalog omitted modelList"))?;
    let available = model_list
        .iter()
        .filter_map(|model| Some(format!("{}/{}", model.get("provider")?.as_str()?, model.get("id")?.as_str()?)))
        .collect::<Vec<_>>();
    let resolution_values = catalog
        .get("patternResolutions")
        .and_then(Value::as_array)
        .ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "Rust model catalog omitted patternResolutions"))?;
    let expected = patterns.as_deref().unwrap_or_default();
    if expected.len() != resolution_values.len() {
        return Err(failure(StatusCode::BAD_GATEWAY, "Rust backend returned incomplete enabledModels resolution"));
    }
    let mut entries = Vec::with_capacity(expected.len());
    for (index, value) in resolution_values.iter().enumerate() {
        let pattern = value.get("pattern").and_then(Value::as_str).unwrap_or_default();
        if pattern != expected[index] {
            return Err(failure(
                StatusCode::BAD_GATEWAY,
                "Rust backend returned enabledModels resolutions in the wrong order",
            ));
        }
        entries.push(Entry {
            pattern: pattern.to_string(),
            matched: value
                .get("matched")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            pin: value.get("pin").and_then(Value::as_str).map(str::to_owned),
        });
    }
    let globs = catalog
        .get("providerGlobs")
        .and_then(Value::as_object)
        .map(|values| {
            values
                .iter()
                .filter_map(|(provider, value)| value.as_str().map(|glob| (provider.clone(), glob.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(Input { patterns, available, entries, globs })
}

async fn load_context(
    state: &AppState,
    cwd: PathBuf,
    override_patterns: Option<Option<Vec<String>>>,
) -> Result<Context, Response> {
    let settings = settings(state, &cwd).await?;
    let patterns = override_patterns.unwrap_or_else(|| {
        settings
            .get("patterns")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).map(str::to_owned).collect())
    });
    let catalog = catalog(state, &cwd, patterns.as_deref().unwrap_or_default()).await?;
    let input = input_from(patterns, &catalog)?;
    Ok(Context {
        cwd,
        scope: settings.get("scope").and_then(Value::as_str).unwrap_or("global").to_string(),
        settings_path: settings.get("settingsPath").and_then(Value::as_str).unwrap_or_default().to_string(),
        input,
        models: catalog.get("modelList").and_then(Value::as_array).cloned().unwrap_or_default(),
        custom: catalog
            .get("customProviders")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        model_error: catalog.get("modelError").and_then(Value::as_str).map(str::to_owned),
    })
}

fn provider(ref_value: &str) -> &str {
    ref_value.split_once('/').map_or(ref_value, |(provider, _)| provider)
}

fn enabled_refs(entries: &[Entry]) -> Vec<String> {
    let mut seen = HashSet::new();
    entries
        .iter()
        .flat_map(|entry| entry.matched.iter())
        .filter(|value| seen.insert(value.to_string()))
        .cloned()
        .collect()
}

fn provider_refs(input: &Input, id: &str) -> Vec<String> {
    input.available.iter().filter(|reference| provider(reference) == id).cloned().collect()
}

fn format_entry(reference: &str, pin: Option<&str>) -> String {
    pin.map_or_else(|| reference.to_string(), |pin| format!("{reference}:{pin}"))
}

fn provider_order(input: &Input) -> Vec<String> {
    let mut seen = HashSet::new();
    input
        .available
        .iter()
        .map(|reference| provider(reference).to_string())
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn materialize(input: &Input) -> Vec<Entry> {
    if !enabled_refs(&input.entries).is_empty() {
        return input.entries.clone();
    }
    let mut entries = input.entries.clone();
    for id in provider_order(input) {
        let refs = provider_refs(input, &id);
        if let Some(glob) = input.globs.get(&id) {
            entries.push(Entry { pattern: glob.clone(), matched: refs, pin: None });
        } else {
            entries.extend(refs.into_iter().map(|reference| Entry {
                pattern: reference.clone(),
                matched: vec![reference],
                pin: None,
            }));
        }
    }
    entries
}

fn collapse_provider(entries: Vec<Entry>, input: &Input, id: &str) -> Vec<Entry> {
    let Some(glob) = input.globs.get(id) else { return entries };
    let refs = provider_refs(input, id);
    if refs.is_empty() {
        return entries;
    }
    let set = refs.iter().cloned().collect::<HashSet<_>>();
    let involved = entries
        .iter()
        .filter(|entry| entry.matched.iter().any(|reference| set.contains(reference)))
        .collect::<Vec<_>>();
    if involved.iter().any(|entry| entry.pin.is_some()) {
        return entries;
    }
    let covered = involved
        .iter()
        .flat_map(|entry| entry.matched.iter())
        .filter(|reference| set.contains(*reference))
        .cloned()
        .collect::<HashSet<_>>();
    if refs.iter().any(|reference| !covered.contains(reference)) {
        return entries;
    }
    let is_subset =
        |entry: &Entry| !entry.matched.is_empty() && entry.matched.iter().all(|reference| set.contains(reference));
    let subsets = entries.iter().filter(|entry| is_subset(entry)).count();
    if subsets < 2 {
        return entries;
    }
    let Some(first) = entries.iter().position(is_subset) else { return entries };
    let wider = involved
        .iter()
        .filter(|entry| !is_subset(entry))
        .flat_map(|entry| entry.matched.iter().cloned())
        .collect::<HashSet<_>>();
    if refs.iter().all(|reference| wider.contains(reference)) {
        return entries;
    }
    let replacement = Entry { pattern: glob.clone(), matched: refs, pin: None };
    entries
        .into_iter()
        .enumerate()
        .filter_map(
            |(index, entry)| {
                if is_subset(&entry) { (index == first).then(|| replacement.clone()) } else { Some(entry) }
            },
        )
        .collect()
}

fn normalize(entries: Vec<Entry>, input: &Input) -> Vec<Entry> {
    provider_order(input).iter().fold(entries, |entries, id| collapse_provider(entries, input, id))
}

fn serialize(entries: &[Entry], input: &Input) -> Option<Vec<String>> {
    let has_stale = entries.iter().any(|entry| entry.matched.is_empty());
    let has_pins = entries.iter().any(|entry| entry.pin.is_some());
    let enabled = enabled_refs(entries).into_iter().collect::<HashSet<_>>();
    let covers_all = input.available.iter().all(|reference| enabled.contains(reference));
    if covers_all && !has_stale && !has_pins {
        None
    } else {
        Some(entries.iter().map(|entry| entry.pattern.clone()).collect())
    }
}

fn edit_set(input: &Input, requested: &[String], enable: bool) -> Result<Option<Vec<String>>, &'static str> {
    let available = input.available.iter().cloned().collect::<HashSet<_>>();
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for reference in requested {
        if available.contains(reference) && seen.insert(reference.clone()) {
            targets.push(reference.clone());
        }
    }
    if targets.is_empty() {
        return Ok(input.patterns.clone());
    }
    let mut entries = materialize(input);
    if enable {
        let mut current = enabled_refs(&entries).into_iter().collect::<HashSet<_>>();
        for reference in targets {
            if current.insert(reference.clone()) {
                entries.push(Entry { pattern: reference.clone(), matched: vec![reference], pin: None });
            }
        }
    } else {
        let removed = targets.into_iter().collect::<HashSet<_>>();
        let mut next = Vec::new();
        for entry in entries {
            if !entry.matched.iter().any(|reference| removed.contains(reference)) {
                next.push(entry);
                continue;
            }
            next.extend(entry.matched.into_iter().filter(|reference| !removed.contains(reference)).map(|reference| {
                Entry {
                    pattern: format_entry(&reference, entry.pin.as_deref()),
                    matched: vec![reference],
                    pin: entry.pin.clone(),
                }
            }));
        }
        if enabled_refs(&next).is_empty() {
            return Err("last-model");
        }
        entries = next;
    }
    Ok(serialize(&normalize(entries, input), input))
}

fn edit_constrain(input: &Input) -> Option<Vec<String>> {
    let providers = provider_order(input).into_iter().collect::<HashSet<_>>();
    let mut entries = Vec::new();
    let mut cut = false;
    for entry in &input.entries {
        let prefix = entry.pattern.split_once('/').map(|(prefix, _)| prefix).unwrap_or("");
        let inside = if providers.contains(prefix) {
            entry.matched.iter().filter(|reference| provider(reference) == prefix).cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if inside.is_empty() || inside.len() == entry.matched.len() {
            entries.push(entry.clone());
            continue;
        }
        cut = true;
        entries.extend(inside.into_iter().map(|reference| Entry {
            pattern: format_entry(&reference, entry.pin.as_deref()),
            matched: vec![reference],
            pin: entry.pin.clone(),
        }));
    }
    if !cut {
        return input.patterns.clone();
    }
    serialize(&entries, input)
}

fn edit_prune(input: &Input) -> Option<Vec<String>> {
    let entries = input.entries.iter().filter(|entry| !entry.matched.is_empty()).cloned().collect::<Vec<_>>();
    if entries.len() == input.entries.len() {
        return input.patterns.clone();
    }
    if entries.is_empty() { None } else { serialize(&entries, input) }
}

fn rename_model_patterns(patterns: &[String], renames: &[Value]) -> Vec<String> {
    let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
    patterns
        .iter()
        .map(|pattern| {
            for rename in renames {
                let Some(from) = rename.get("from").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
                    continue;
                };
                let Some(to) = rename.get("to").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
                    continue;
                };
                if from == to {
                    continue;
                }
                if pattern == from {
                    return to.to_string();
                }
                if let Some(suffix) = pattern.strip_prefix(&format!("{from}:")) {
                    if levels.contains(&suffix) {
                        return format!("{to}:{suffix}");
                    }
                }
            }
            pattern.clone()
        })
        .collect()
}

fn rename_provider_patterns(patterns: &[String], renames: &[Value]) -> Vec<String> {
    patterns
        .iter()
        .map(|pattern| {
            for rename in renames {
                let Some(from) = rename.get("from").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
                    continue;
                };
                let Some(to) = rename.get("to").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
                    continue;
                };
                if from != to && pattern.starts_with(&format!("{from}/")) {
                    return format!("{to}{}", &pattern[from.len()..]);
                }
            }
            pattern.clone()
        })
        .collect()
}

fn same(a: &Option<Vec<String>>, b: &Option<Vec<String>>) -> bool {
    a == b
}

fn view(context: &Context) -> Value {
    let enabled = enabled_refs(&context.input.entries).into_iter().collect::<HashSet<_>>();
    let all_enabled = enabled.is_empty();
    let mut pins = HashMap::<String, String>::new();
    for entry in &context.input.entries {
        if let Some(pin) = &entry.pin {
            for reference in &entry.matched {
                pins.entry(reference.clone()).or_insert_with(|| pin.clone());
            }
        }
    }
    let stale = context
        .input
        .entries
        .iter()
        .filter(|entry| entry.matched.is_empty())
        .map(|entry| entry.pattern.clone())
        .collect::<Vec<_>>();
    let mut providers = HashMap::<String, Vec<Value>>::new();
    let mut provider_names = HashMap::<String, String>::new();
    for model in &context.models {
        let Some(id) = model.get("id").and_then(Value::as_str) else { continue };
        let Some(provider_id) = model.get("provider").and_then(Value::as_str) else { continue };
        let reference = format!("{provider_id}/{id}");
        let name = model.get("name").and_then(Value::as_str).filter(|name| !name.is_empty()).unwrap_or(id);
        let mut item =
            json!({"id":id,"name":name,"ref":reference,"enabled":all_enabled || enabled.contains(&reference)});
        if let Some(window) = model.get("contextWindow").filter(|value| !value.is_null()) {
            item["contextWindow"] = window.clone();
        }
        if let Some(pin) = pins.get(&reference) {
            item["thinkingPin"] = json!(pin);
        }
        providers.entry(provider_id.to_string()).or_default().push(item);
        provider_names.insert(
            provider_id.to_string(),
            model.get("providerName").and_then(Value::as_str).unwrap_or(provider_id).to_string(),
        );
    }
    let mut provider_values = providers.into_iter().map(|(id, mut models)| {
        models.sort_by(|a,b| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()).then_with(|| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or(""))));
        let enabled_count = models.iter().filter(|model| model["enabled"] == true).count();
        json!({"id":id,"name":provider_names.get(&id).cloned().unwrap_or_else(|| id.clone()),"kind":if context.custom.contains(&id) { "custom" } else { "builtin" },"enabledCount":enabled_count,"models":models})
    }).collect::<Vec<_>>();
    provider_values.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
            .then_with(|| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or("")))
    });
    let enabled_total = if all_enabled {
        context.input.available.len()
    } else {
        context.input.available.iter().filter(|reference| enabled.contains(*reference)).count()
    };
    let mut result = json!({
        "allEnabled":all_enabled,
        "patterns":context.input.patterns,
        "stalePatterns":stale,
        "enabledTotal":enabled_total,
        "availableTotal":context.input.available.len(),
        "providers":provider_values,
        "scope":context.scope,
        "settingsPath":context.settings_path,
        "editable":context.scope == "global",
    });
    if let Some(error) = &context.model_error {
        result["modelError"] = json!(error);
    }
    result
}

async fn save(state: &AppState, cwd: &Path, patterns: Option<Vec<String>>) -> Result<(), Response> {
    let response = enabled_settings::put(state, json!({"cwd":cwd.display().to_string(),"patterns":patterns}));
    response_json(response).await.map(|_| ())
}

pub async fn get(State(state): State<AppState>, Query(query): Query<HashMap<String, String>>) -> Response {
    let cwd = match resolve_cwd(&state, query.get("cwd").map(String::as_str)) {
        Ok(cwd) => cwd,
        Err(response) => return response,
    };
    match load_context(&state, cwd, None).await {
        Ok(context) => Json(view(&context)).into_response(),
        Err(response) => response,
    }
}

pub async fn put(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(op) = body.get("op").and_then(Value::as_str) else {
        return failure(StatusCode::BAD_REQUEST, "Invalid op");
    };
    if !["models", "provider", "clear", "prune", "resync"].contains(&op) {
        return failure(StatusCode::BAD_REQUEST, "Invalid op");
    }
    if matches!(op, "models" | "provider") && !body.get("enabled").is_some_and(Value::is_boolean) {
        return failure(StatusCode::BAD_REQUEST, "enabled must be a boolean");
    }
    if body.get("cwd").is_some_and(|value| !value.is_string()) {
        return failure(StatusCode::BAD_REQUEST, "Invalid cwd");
    }
    let cwd = match resolve_cwd(&state, body.get("cwd").and_then(Value::as_str)) {
        Ok(cwd) => cwd,
        Err(response) => return response,
    };
    let mut context = match load_context(&state, cwd, None).await {
        Ok(context) => context,
        Err(response) => return response,
    };
    if context.scope == "project" {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Project settings override enabledModels","reason":"project-scope"})),
        )
            .into_response();
    }
    let original = context.input.patterns.clone();
    let next = match op {
        "clear" => None,
        "prune" => edit_prune(&context.input),
        "models" | "provider" => {
            let refs = if op == "provider" {
                let Some(provider) = body.get("provider").and_then(Value::as_str).filter(|value| !value.is_empty())
                else {
                    return failure(StatusCode::BAD_REQUEST, "provider is required");
                };
                provider_refs(&context.input, provider)
            } else {
                let Some(refs) =
                    body.get("refs").and_then(Value::as_array).filter(|refs| refs.iter().all(Value::is_string))
                else {
                    return failure(StatusCode::BAD_REQUEST, "refs must be an array of strings");
                };
                refs.iter().filter_map(Value::as_str).map(str::to_owned).collect()
            };
            match edit_set(&context.input, &refs, body["enabled"] == true) {
                Ok(patterns) => patterns,
                Err(_) => {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({"error":"At least one model must stay enabled","reason":"last-model"})),
                    )
                        .into_response();
                }
            }
        }
        "resync" => {
            let empty = Value::Array(Vec::new());
            let Some(renames) = body.get("renames").and_then(Value::as_array) else {
                return failure(StatusCode::BAD_REQUEST, "Invalid resync payload");
            };
            let model_renames_value = body.get("modelRenames").filter(|value| !value.is_null()).unwrap_or(&empty);
            let Some(model_renames) = model_renames_value.as_array() else {
                return failure(StatusCode::BAD_REQUEST, "Invalid resync payload");
            };
            let fully_enabled_value = body.get("fullyEnabled").filter(|value| !value.is_null()).unwrap_or(&empty);
            let Some(fully_enabled) =
                fully_enabled_value.as_array().filter(|values| values.iter().all(Value::is_string))
            else {
                return failure(StatusCode::BAD_REQUEST, "Invalid resync payload");
            };
            if !renames.iter().chain(model_renames).all(|rename| {
                rename.get("from").and_then(Value::as_str).is_some_and(|value| !value.is_empty())
                    && rename.get("to").and_then(Value::as_str).is_some_and(|value| !value.is_empty())
            }) {
                return failure(StatusCode::BAD_REQUEST, "Invalid resync payload");
            }
            let patterns = context
                .input
                .patterns
                .as_ref()
                .map(|original| rename_provider_patterns(&rename_model_patterns(original, model_renames), renames));
            if !same(&patterns, &context.input.patterns) {
                context = match load_context(&state, context.cwd.clone(), Some(patterns)).await {
                    Ok(context) => context,
                    Err(response) => return response,
                };
            }
            let constrained = edit_constrain(&context.input);
            if !same(&constrained, &context.input.patterns) {
                context = match load_context(&state, context.cwd.clone(), Some(constrained)).await {
                    Ok(context) => context,
                    Err(response) => return response,
                };
            }
            let refs = fully_enabled
                .iter()
                .filter_map(Value::as_str)
                .map(|id| {
                    renames
                        .iter()
                        .find(|rename| rename.get("from").and_then(Value::as_str) == Some(id))
                        .and_then(|rename| rename.get("to"))
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                })
                .flat_map(|id| {
                    context.models.iter().filter_map(move |model| {
                        (model.get("provider").and_then(Value::as_str) == Some(id))
                            .then(|| format!("{id}/{}", model.get("id").and_then(Value::as_str).unwrap_or("")))
                    })
                })
                .collect::<Vec<_>>();
            match edit_set(&context.input, &refs, true) {
                Ok(patterns) => patterns,
                Err(_) => {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({"error":"At least one model must stay enabled","reason":"last-model"})),
                    )
                        .into_response();
                }
            }
        }
        _ => unreachable!(),
    };
    if !same(&next, &original) {
        if let Err(response) = save(&state, &context.cwd, next.clone()).await {
            return response;
        }
        context = match load_context(&state, context.cwd.clone(), Some(next)).await {
            Ok(context) => context,
            Err(response) => return response,
        };
    }
    Json(view(&context)).into_response()
}
