//! Sidebar-only session/project state stored beside the agent configuration.
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::web_backend::{AppState, failure};

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IDS: usize = 500;
const MAX_PROJECT_STRING: usize = 4096;
const MAX_ORDER: usize = 1000;
const MAX_READS: usize = 2000;
const MAX_NAMES: usize = 1000;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn path(state: &AppState) -> PathBuf {
    state.agent_dir.join("pi-web-session-state.json")
}
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
}
fn empty() -> Value {
    json!({"version":1,"revision":0,"sessions":{},"projects":{}})
}
fn failure_response(status: StatusCode, reason: &str, error: impl Into<String>) -> Response {
    (status, Json(json!({"reason":reason,"error":error.into()}))).into_response()
}
fn valid_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 4096
        && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}
fn valid_key(text: &str) -> bool {
    !text.is_empty() && text.chars().count() <= MAX_PROJECT_STRING && text != "__proto__"
}
fn timestamp(v: &Value) -> bool {
    v.as_u64().is_some() || v.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0)
}
fn project_name(v: &Value) -> Option<Option<String>> {
    if v.is_null() {
        return Some(None);
    }
    let s = v.as_str()?;
    let name = s.trim();
    if name.is_empty() {
        return Some(None);
    }
    if name.chars().count() > 80
        || name
            .chars()
            .any(|c| (c as u32) <= 0x1f || (0x7f..=0x9f).contains(&(c as u32)) || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return None;
    }
    Some(Some(name.to_owned()))
}
fn valid_list<'a>(v: &'a Value, field: &str, allow_empty: bool) -> Result<Vec<&'a str>, String> {
    let arr = v
        .as_array()
        .ok_or_else(|| format!("{field} must be {}array", if allow_empty { "an " } else { "a non-empty " }))?;
    if (!allow_empty && arr.is_empty()) || arr.len() > MAX_IDS {
        return Err(format!("{field} may hold at most {MAX_IDS} projectKeys"));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in arr {
        let Some(s) = item.as_str().filter(|s| valid_key(s)) else {
            return Err(format!("{field} must hold non-empty strings of at most {MAX_PROJECT_STRING} characters"));
        };
        if seen.insert(s) {
            out.push(s);
        }
    }
    Ok(out)
}
fn parse_request(body: &Value) -> Result<(), String> {
    let obj = body.as_object().ok_or("Expected a JSON object")?;
    match obj.get("action").and_then(Value::as_str).unwrap_or("") {
        "set" | "read" => {
            let ids = obj.get("ids").and_then(Value::as_array).ok_or("ids must be a non-empty array")?;
            if ids.is_empty() { return Err("ids must be a non-empty array".into()); }
            if ids.len() > MAX_IDS { return Err(format!("ids may hold at most {MAX_IDS} session ids")); }
            if ids.iter().any(|v| !v.as_str().is_some_and(valid_id)) { return Err("ids must be session ids".into()); }
            if obj["action"] == "set" {
                let p = obj.get("pinned"); let a = obj.get("archived");
                if p.is_some() == a.is_some() { return Err("Send exactly one of pinned or archived".into()); }
                if !(p.or(a).unwrap().is_boolean()) { return Err(format!("{} must be a boolean", if p.is_some(){"pinned"}else{"archived"})); }
            }
        }
        "restore" => {
            let entries = obj.get("entries").and_then(Value::as_array).ok_or("entries must be an array")?;
            if entries.len() > MAX_IDS { return Err(format!("entries may hold at most {MAX_IDS} sessions")); }
            let mut seen=HashSet::new();
            for e in entries { let Some(m)=e.as_object() else{return Err("Each entry needs a session id".into())}; let Some(id)=m.get("id").and_then(Value::as_str).filter(|s|valid_id(s)) else{return Err("Each entry needs a session id".into())};
                if !seen.insert(id){return Err("entries must name each session once".into())}
                for k in ["pinnedAt","archivedAt"] { let v=m.get(k).ok_or("pinnedAt and archivedAt must be timestamps or null")?; if !v.is_null()&&!timestamp(v){return Err("pinnedAt and archivedAt must be timestamps or null".into())} }
            }
        }
        "pin-project" => { for k in ["projectKey","root"] {if !obj.get(k).and_then(Value::as_str).is_some_and(valid_key){return Err(format!("{k} must be a non-empty string of at most {MAX_PROJECT_STRING} characters"))}} if !obj.get("pinned").is_some_and(Value::is_boolean){return Err("pinned must be a boolean".into())} }
        "add-projects" => { valid_list(obj.get("keys").unwrap_or(&Value::Null),"keys",false)?; }
        "move-project" => { let k=obj.get("projectKey").and_then(Value::as_str).filter(|s|valid_key(s)); let a=obj.get("anchorKey").and_then(Value::as_str).filter(|s|valid_key(s)); if k.is_none()||a.is_none(){return Err(format!("projectKey and anchorKey must be non-empty strings of at most {MAX_PROJECT_STRING} characters"))} if k==a{return Err("projectKey and anchorKey must differ".into())} if !matches!(obj.get("position").and_then(Value::as_str),Some("before"|"after")){return Err("position must be \"before\" or \"after\"".into())} if let Some(add)=obj.get("add"){valid_list(add,"add",true)?;} }
        "rename-project" => {if !obj.get("projectKey").and_then(Value::as_str).is_some_and(valid_key){return Err(format!("projectKey must be a non-empty string of at most {MAX_PROJECT_STRING} characters"))} if project_name(obj.get("name").unwrap_or(&Value::Null)).is_none(){return Err("name must be null or one line of at most 80 characters".into())}}
        _ => return Err("action must be \"set\", \"restore\", \"pin-project\", \"add-projects\", \"move-project\", \"rename-project\" or \"read\"".into()),
    }
    Ok(())
}
fn normalize(mut v: Value) -> Value {
    if !v.is_object()
        || v.get("version").is_some_and(|x| x != 1)
        || v.get("sessions").is_some_and(|x| !x.is_object())
        || v.get("projects").is_some_and(|x| !x.is_object())
    {
        return empty();
    }
    let o = v.as_object_mut().unwrap();
    o.insert("version".into(), json!(1));
    if !o.get("revision").is_some_and(|r| r.as_u64().is_some()) {
        o.insert("revision".into(), json!(0));
    }
    for field in ["sessions", "projects"] {
        if !o.contains_key(field) {
            o.insert(field.into(), json!({}));
        }
    }
    if let Some(sessions) = o.get_mut("sessions").and_then(Value::as_object_mut) {
        sessions.retain(|id, e| valid_id(id) && e.is_object());
        for e in sessions.values_mut() {
            let m = e.as_object_mut().unwrap();
            m.retain(|k, v| matches!(k.as_str(), "pinnedAt" | "archivedAt") && timestamp(v));
        }
    }
    if let Some(projects) = o.get_mut("projects").and_then(Value::as_object_mut) {
        projects.retain(|k, e| {
            valid_key(k)
                && e.get("root").and_then(Value::as_str).is_some_and(valid_key)
                && e.get("pinnedAt").is_some_and(timestamp)
        });
    }
    if let Some(arr) = o.get_mut("projectOrder").and_then(Value::as_array_mut) {
        let mut seen = HashSet::new();
        arr.retain(|v| v.as_str().is_some_and(|s| valid_key(s) && seen.insert(s.to_owned())));
        if arr.len() > MAX_ORDER {
            arr.truncate(MAX_ORDER)
        }
    }
    if let Some(names) = o.get_mut("projectNames").and_then(Value::as_object_mut) {
        names.retain(|k, v| valid_key(k) && project_name(v).is_some_and(|n| n.is_some()));
        for v in names.values_mut() {
            if let Some(Some(n)) = project_name(v) {
                *v = json!(n)
            }
        }
        if names.len() > MAX_NAMES {
            names.clear()
        }
    }
    if let Some(reads) = o.get_mut("readAt").and_then(Value::as_object_mut) {
        reads.retain(|id, v| valid_id(id) && timestamp(v));
        if reads.len() > MAX_READS {
            let mut vals: Vec<_> = reads.iter().map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0))).collect();
            vals.sort_by(|a, b| b.1.total_cmp(&a.1));
            let keep: HashSet<_> = vals.into_iter().take(MAX_READS).map(|x| x.0).collect();
            reads.retain(|k, _| keep.contains(k));
        }
    }
    v
}
fn load(p: &Path) -> Result<Value, String> {
    match fs::metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty()),
        Err(e) => Err(e.to_string()),
        Ok(m) if !m.is_file() || m.len() > MAX_BYTES => {
            Err("Session UI state is not a regular file or exceeds 4 MiB".into())
        }
        Ok(_) => {
            let b = fs::read(p).map_err(|e| e.to_string())?;
            Ok(normalize(serde_json::from_slice(&b).unwrap_or(Value::Null)))
        }
    }
}
fn atomic_write(p: &Path, v: &Value) -> Result<(), String> {
    let parent = p.parent().ok_or("invalid state path")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".pi-web-session-state.{}.{}.tmp", std::process::id(), id));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp).map_err(|e| e.to_string())?;
        f.write_all(format!("{}\n", serde_json::to_string_pretty(v).map_err(|e| e.to_string())?).as_bytes())
            .map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        #[cfg(windows)]
        if p.exists() {
            fs::remove_file(p).map_err(|e| e.to_string())?;
        }
        fs::rename(&tmp, p).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
fn write_locked(p: &Path, body: &Value) -> Result<Value, String> {
    let _guard = WRITE_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner());
    fs::create_dir_all(p.parent().ok_or("invalid state path")?).map_err(|e| e.to_string())?;
    let lock = p.with_extension("json.lock");
    let started = std::time::Instant::now();
    let _lockfile = loop {
        match OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && started.elapsed() < Duration::from_secs(3) => {
                thread::sleep(Duration::from_millis(40))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err("Another process holds the session UI state lock".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let mut s = load(p)?;
    let before = s.clone();
    let now = now_ms();
    let action = body["action"].as_str().unwrap_or("");
    let mut changed = false;
    match action {
        "set" => {
            for id in body["ids"].as_array().unwrap() {
                let id = id.as_str().unwrap();
                let sessions = s["sessions"].as_object_mut().unwrap();
                let entry = sessions.entry(id).or_insert(json!({}));
                let e = entry.as_object_mut().unwrap();
                if let Some(pin) = body.get("pinned") {
                    if pin == false {
                        changed |= e.remove("pinnedAt").is_some()
                    } else if !e.contains_key("pinnedAt") || e.contains_key("archivedAt") {
                        e.insert("pinnedAt".into(), json!(now));
                        e.remove("archivedAt");
                        changed = true
                    }
                } else if body["archived"] == true {
                    e.insert("archivedAt".into(), json!(now));
                    changed = true;
                    e.remove("pinnedAt");
                } else {
                    changed |= e.remove("archivedAt").is_some()
                }
                if e.is_empty() {
                    sessions.remove(id);
                }
            }
        }
        "restore" => {
            for item in body["entries"].as_array().unwrap() {
                let id = item["id"].as_str().unwrap();
                let mut e = Map::new();
                for k in ["pinnedAt", "archivedAt"] {
                    if !item[k].is_null() {
                        e.insert(k.into(), item[k].clone());
                    }
                }
                let sessions = s["sessions"].as_object_mut().unwrap();
                if e.is_empty() {
                    changed |= sessions.remove(id).is_some()
                } else if sessions.get(id) != Some(&Value::Object(e.clone())) {
                    sessions.insert(id.into(), Value::Object(e));
                    changed = true
                }
            }
        }
        "pin-project" => {
            let key = body["projectKey"].as_str().unwrap();
            let was_pinned = s["projects"].get(key).is_some();
            let projects = s["projects"].as_object_mut().unwrap();
            let rebanded = if body["pinned"] == true {
                let v = json!({"pinnedAt":now,"root":body["root"]});
                if projects.get(key) != Some(&v) {
                    projects.insert(key.into(), v);
                    changed = true
                }
                !was_pinned
            } else {
                changed |= projects.remove(key).is_some();
                was_pinned
            };
            if rebanded {
                if let Some(order) = s.get("projectOrder").and_then(Value::as_array) {
                    let mut list: Vec<String> =
                        order.iter().filter_map(Value::as_str).map(str::to_owned).filter(|item| item != key).collect();
                    if !list.iter().any(|item| item == key) && list.len() >= MAX_ORDER { /* keep the project unsaved */
                    } else {
                        let projects = s["projects"].as_object().unwrap();
                        let index = if body["pinned"] == true {
                            list.iter()
                                .enumerate()
                                .filter(|(_, item)| projects.contains_key(*item))
                                .map(|(i, _)| i + 1)
                                .last()
                                .unwrap_or(0)
                        } else {
                            list.iter().position(|item| !projects.contains_key(item)).unwrap_or(list.len())
                        };
                        list.insert(index, key.to_owned());
                        let next: Vec<Value> = list.into_iter().map(Value::String).collect();
                        if s["projectOrder"] != json!(next) {
                            s["projectOrder"] = json!(next);
                            changed = true;
                        }
                    }
                }
            }
        }
        "add-projects" | "move-project" => {
            let order = s.get("projectOrder").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut list: Vec<String> = order.iter().filter_map(Value::as_str).map(str::to_owned).collect();
            if action == "add-projects" {
                let mut prepend = Vec::new();
                for k in body["keys"].as_array().unwrap().iter().filter_map(Value::as_str) {
                    if !list.iter().any(|x| x == k) && !prepend.iter().any(|x| x == k) {
                        prepend.push(k.to_owned())
                    }
                }
                if list.len() >= MAX_ORDER {
                    prepend.clear();
                } else {
                    prepend.truncate(MAX_ORDER - list.len());
                }
                prepend.extend(list);
                list = prepend;
            } else {
                let add = body["add"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|k| !list.iter().any(|x| x == *k))
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let mut expanded = add;
                expanded.extend(list);
                list = expanded;
                let key = body["projectKey"].as_str().unwrap();
                list.retain(|x| x != key);
                let anchor = body["anchorKey"].as_str().unwrap();
                let index = list.iter().position(|x| x == anchor).unwrap_or_else(|| {
                    list.insert(0, anchor.into());
                    0
                });
                list.insert(if body["position"] == "before" { index } else { index + 1 }, key.into());
            }
            let next: Vec<Value> = if action == "add-projects" {
                list.into_iter().map(Value::String).collect()
            } else {
                list.into_iter().take(MAX_ORDER).map(Value::String).collect()
            };
            if next != order {
                changed = true;
                if next.is_empty() {
                    s.as_object_mut().unwrap().remove("projectOrder");
                } else {
                    s["projectOrder"] = json!(next)
                }
            }
        }
        "rename-project" => {
            let key = body["projectKey"].as_str().unwrap();
            let mut names = s.get("projectNames").and_then(Value::as_object).cloned().unwrap_or_default();
            let name = project_name(&body["name"]).unwrap().map(Value::String);
            let prev = names.get(key).cloned();
            if prev != name {
                if let Some(n) = name {
                    names.insert(key.into(), n);
                } else {
                    names.remove(key);
                }
                if names.len() <= MAX_NAMES
                    && serde_json::to_vec(&Value::Object(names.clone()))
                        .map_or(false, |bytes| bytes.len() <= 256 * 1024)
                {
                    changed = true;
                    if names.is_empty() {
                        s.as_object_mut().unwrap().remove("projectNames");
                    } else {
                        s["projectNames"] = Value::Object(names)
                    }
                }
            }
        }
        "read" => {
            let reads = s.as_object_mut().unwrap().entry("readAt").or_insert(json!({})).as_object_mut().unwrap();
            for id in body["ids"].as_array().unwrap().iter().filter_map(Value::as_str) {
                if reads.get(id).and_then(Value::as_u64).unwrap_or(0) < now {
                    reads.insert(id.into(), json!(now));
                    changed = true
                }
            }
            if reads.len() > MAX_READS {
                let mut vals: Vec<_> = reads.iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0))).collect();
                vals.sort_by_key(|x| std::cmp::Reverse(x.1));
                let keep: HashSet<_> = vals.into_iter().take(MAX_READS).map(|x| x.0).collect();
                reads.retain(|k, _| keep.contains(k));
            }
        }
        _ => {}
    }
    if changed {
        s["revision"] = json!(before["revision"].as_u64().unwrap_or(0) + 1);
        atomic_write(p, &s)?;
    }
    drop(_lockfile);
    let _ = fs::remove_file(lock);
    Ok(s)
}

pub async fn get(State(state): State<AppState>) -> Response {
    match load(&path(&state)) {
        Ok(v) => (StatusCode::OK, [(axum::http::header::CACHE_CONTROL, "no-store")], Json(json!({"state":v})))
            .into_response(),
        Err(e) => failure(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
pub async fn post(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    if let Err(error) = parse_request(&body) {
        return failure_response(StatusCode::BAD_REQUEST, "invalid-request", error);
    }
    match write_locked(&path(&state), &body) {
        Ok(v) => {
            if body["action"] == "rename-project"
                && !body["name"].is_null()
                && v["projectNames"][body["projectKey"].as_str().unwrap_or("")] != body["name"]
            {
                return failure_response(
                    StatusCode::CONFLICT,
                    "full",
                    "No room for another project name (at most 1000, 256 KiB in all)",
                );
            }
            (StatusCode::OK, [(axum::http::header::CACHE_CONTROL, "no-store")], Json(json!({"state":v})))
                .into_response()
        }
        Err(e) => {
            let locked = e.contains("holds the session UI state lock");
            failure_response(
                if locked { StatusCode::CONFLICT } else { StatusCode::INTERNAL_SERVER_ERROR },
                if locked { "locked" } else { "internal" },
                e,
            )
        }
    }
}

pub fn revision(agent_dir: &Path) -> u64 {
    load(&agent_dir.join("pi-web-session-state.json")).ok().and_then(|s| s["revision"].as_u64()).unwrap_or(0)
}
pub fn forget(agent_dir: &Path, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let p = agent_dir.join("pi-web-session-state.json");
    let mut s = match load(&p) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut changed = false;
    for id in ids {
        changed |= s["sessions"].as_object_mut().is_some_and(|m| m.remove(id).is_some());
        changed |= s["readAt"].as_object_mut().is_some_and(|m| m.remove(id).is_some());
    }
    if changed {
        s["revision"] = json!(s["revision"].as_u64().unwrap_or(0) + 1);
        let _ = atomic_write(&p, &s);
    }
}
