use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::web_backend::{AppState, failure};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn models_path(state: &AppState) -> PathBuf {
    state.agent_dir.join("models.json")
}

fn strip_jsonc(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    let mut in_string = false;
    while let Some(ch) = chars.next() {
        if in_string {
            stripped.push(ch);
            match ch {
                '\\' => {
                    if let Some(next) = chars.next() {
                        stripped.push(next);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            stripped.push(ch);
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    stripped.push('\n');
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = ' ';
            let mut closed = false;
            for next in chars.by_ref() {
                if previous == '*' && next == '/' {
                    closed = true;
                    break;
                }
                previous = next;
            }
            if closed {
                stripped.push(' ');
            } else {
                stripped.push_str("/*");
            }
            continue;
        }
        stripped.push(ch);
    }
    stripped
}

// Remove commas before a closing array/object while keeping string contents intact.
fn remove_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut output = String::with_capacity(text.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            output.push(ch);
            if ch == '\\' && index + 1 < chars.len() {
                index += 1;
                output.push(chars[index]);
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            output.push(ch);
        } else if ch == ',' {
            let mut next = index + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next == chars.len() || !matches!(chars[next], '}' | ']') {
                output.push(ch);
            }
        } else {
            output.push(ch);
        }
        index += 1;
    }
    output
}

fn parse_config(path: &Path) -> Result<Value, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(json!({"providers":{}})),
        Err(error) => return Err(format!("Failed to read {}: {error}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(json!({"providers":{}}));
    }
    let cleaned = remove_trailing_commas(&strip_jsonc(&text));
    let parsed: Value =
        serde_json::from_str(&cleaned).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if !parsed.is_object() {
        return Err(format!("Failed to read {}: expected a JSON object", path.display()));
    }
    Ok(parsed)
}

fn normalize(mut value: Value) -> Value {
    let Some(providers) = value.get_mut("providers").and_then(Value::as_object_mut) else { return value };
    for provider in providers.values_mut() {
        let Some(models) = provider.get_mut("models").and_then(Value::as_array_mut) else { continue };
        models.retain(|model| {
            !model.is_object() || model.get("id").and_then(Value::as_str).is_none_or(|id| !id.trim().is_empty())
        });
        for model in models.iter_mut() {
            let Some(cost) = model.get_mut("cost") else { continue };
            let Some(object) = cost.as_object_mut() else { continue };
            let keys = ["input", "output", "cacheRead", "cacheWrite"];
            let provided = keys.iter().any(|key| object.contains_key(*key));
            let valid = provided
                && !keys.iter().any(|key| {
                    object.get(*key).is_some_and(|value| {
                        !value.is_number() || value.as_f64().is_none_or(|number| !number.is_finite())
                    })
                });
            if valid {
                for key in keys {
                    object.entry(key).or_insert(Value::from(0));
                }
            }
        }
        for model in models.iter_mut() {
            let remove = model.get("cost").is_some_and(|cost| {
                !cost.is_object() || {
                    let keys = ["input", "output", "cacheRead", "cacheWrite"];
                    !keys.iter().any(|key| cost.get(*key).is_some())
                        || keys.iter().any(|key| {
                            cost.get(*key).is_some_and(|value| {
                                !value.is_number() || value.as_f64().is_none_or(|number| !number.is_finite())
                            })
                        })
                }
            });
            if remove {
                if let Some(object) = model.as_object_mut() {
                    object.remove("cost");
                }
            }
        }
    }
    value
}

fn write_atomic(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let destination = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(|_| format!("Failed to write {}: dangling symbolic link", path.display()))?
        }
        _ => path.to_path_buf(),
    };
    let parent = destination.parent().ok_or_else(|| format!("Invalid path: {}", destination.display()))?;
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".models.json.{}.{}.tmp", std::process::id(), id));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|error| error.to_string())?;
        file.write_all(serde_json::to_string_pretty(value).map_err(|error| error.to_string())?.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        #[cfg(windows)]
        if destination.exists() {
            fs::remove_file(&destination).map_err(|error| error.to_string())?;
        }
        fs::rename(&temp, &destination).map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|error: String| format!("Failed to write {}: {error}", path.display()))
}

pub async fn get(State(state): State<AppState>) -> Response {
    let path = models_path(&state);
    match parse_config(&path) {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error":error}))).into_response(),
    }
}

pub async fn put(State(state): State<AppState>, Json(value): Json<Value>) -> Response {
    if !value.is_object() {
        return failure(StatusCode::BAD_REQUEST, "Expected a JSON object");
    }
    let path = models_path(&state);
    if let Err(error) = parse_config(&path) {
        return (StatusCode::CONFLICT, Json(json!({"error":error}))).into_response();
    }
    let normalized = normalize(value);
    match write_atomic(&path, &normalized) {
        Ok(()) => Json(json!({"success":true})).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}
