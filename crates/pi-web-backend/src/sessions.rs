use super::*;

use crate::app_state::{AppState, WebSession, failure, lock, ok};
use crate::rpc_worker::{RpcSessionWorker, RpcWorkerOptions, SESSION_IDLE_TIMEOUT, session_tool_selection};
use pi_coding_agent::rpc::RpcCommand;

fn wrap_rpc_worker(worker: Arc<RpcSessionWorker>) -> WebSession {
    let events = worker.events.clone();
    let ui = worker.ui.clone();
    WebSession { rpc_worker: worker, events, ui, created_at: pi_ai::now_ms() }
}

async fn insert_rpc_worker(state: &AppState, worker: Arc<RpcSessionWorker>) -> WebSession {
    let session = wrap_rpc_worker(worker);
    let id = session.session_id().await;
    lock(&state.sessions).insert(id, session.clone());
    session
}

pub(super) async fn reap_idle_sessions(state: AppState) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = state.stop.cancelled() => return,
            _ = interval.tick() => {}
        }
        let candidates = lock(&state.sessions)
            .iter()
            .filter(|(_, session)| session.rpc_worker.last_activity().elapsed() >= SESSION_IDLE_TIMEOUT)
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect::<Vec<_>>();
        futures::StreamExt::for_each_concurrent(stream::iter(candidates), 8, |(id, session)| {
            close_idle_session(&state, id, session)
        })
        .await;
    }
}

async fn close_idle_session(state: &AppState, id: String, session: WebSession) {
    let worker = &session.rpc_worker;
    let last_activity = worker.last_activity();
    if last_activity.elapsed() < SESSION_IDLE_TIMEOUT || session.ui.has_background_work() || session.ui.awaiting_input()
    {
        return;
    }
    if worker.ensure_alive().is_ok() {
        let Ok(status) = worker.refresh_state().await else {
            return;
        };
        if status.session_id != id
            || status.is_streaming
            || status.is_bash_running
            || status.is_compacting
            || status.pending_message_count != 0
        {
            return;
        }
    }
    // Serialize reopening with disposal so two workers cannot own the same file.
    let opening = session_opening_lock(state, &id);
    let _opening = opening.lock().await;
    let removed = {
        let mut sessions = lock(&state.sessions);
        if !sessions.get(&id).is_some_and(|current| Arc::ptr_eq(&current.rpc_worker, worker))
            // The map and this sweep own one reference each. An active request owns another.
            || Arc::strong_count(worker) != 2
            || worker.last_activity() != last_activity
            || session.ui.has_background_work()
            || session.ui.awaiting_input()
        {
            return;
        }
        sessions.remove(&id)
    };
    if let Some(removed) = removed {
        let _ = removed.events.send(json!({"type":"session_ended","sessionId":id,"reason":"idle_timeout"}));
        removed.dispose().await;
    }
}

fn session_opening_lock(state: &AppState, id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = lock(&state.opening_sessions);
    locks.retain(|_, weak| weak.strong_count() > 0);
    match locks.get(id).and_then(Weak::upgrade) {
        Some(opening) => opening,
        None => {
            let opening = Arc::new(tokio::sync::Mutex::new(()));
            locks.insert(id.to_string(), Arc::downgrade(&opening));
            opening
        }
    }
}

fn rpc_status_json(
    ui: &WebExtensionUi,
    state: &pi_coding_agent::rpc::RpcSessionState,
    include_system_prompt: bool,
) -> Value {
    let mut value = serde_json::to_value(state).unwrap_or(Value::Null);
    value["isPromptRunning"] = json!(state.is_streaming);
    value["isBashRunning"] = json!(state.is_bash_running);
    value["queuedMessages"] = json!({"steering":state.steering_messages, "followUp":state.follow_up_messages});
    value["extensionStatuses"] = json!(ui.statuses());
    value["extensionWidgets"] = json!(ui.widgets());
    value["contextUsage"] = state.context_usage.clone().unwrap_or(Value::Null);
    if include_system_prompt {
        value["systemPrompt"] = state.system_prompt.clone().map_or(Value::Null, Value::String);
    } else if let Some(object) = value.as_object_mut() {
        object.remove("systemPrompt");
    }
    value
}

async fn get_or_open(state: &AppState, id: &str) -> anyhow::Result<WebSession> {
    let existing = { lock(&state.sessions).get(id).cloned() };
    if let Some(session) = existing {
        session.rpc_worker.ensure_alive()?;
        return Ok(session);
    }
    let opening = session_opening_lock(state, id);
    let _opening = opening.lock().await;
    let existing = { lock(&state.sessions).get(id).cloned() };
    if let Some(session) = existing {
        session.rpc_worker.ensure_alive()?;
        return Ok(session);
    }
    let info = list_all_sessions(None, &state.agent_dir)
        .into_iter()
        .find(|session| session.id == id)
        .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
    let path = info.path;
    let cwd = if info.cwd.is_empty() { std::env::current_dir()? } else { PathBuf::from(info.cwd) };
    let worker = RpcSessionWorker::open_session_file(&path, cwd, state.agent_dir.clone()).await?;
    Ok(insert_rpc_worker(state, Arc::new(worker)).await)
}

pub(super) async fn create_session(State(state): State<AppState>, Json(input): Json<CreateSessionRequest>) -> Response {
    if input.cwd.trim().is_empty() {
        return failure(StatusCode::BAD_REQUEST, "cwd is required");
    }
    let cwd = crate::cwd::normalize(&input.cwd);
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, format!("cwd does not exist: {}", cwd.display()));
    }
    let thinking_level = match input
        .thinking_level
        .as_deref()
        .map(|level| serde_json::from_value(json!(level)).map_err(|error| format!("Invalid thinking level: {error}")))
        .transpose()
    {
        Ok(level) => level,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error),
    };
    let worker = match RpcSessionWorker::start(RpcWorkerOptions {
        cwd: cwd.clone(),
        session_file: None,
        agent_dir: state.agent_dir.clone(),
        provider: input.provider.clone(),
        model: input.model_id.clone(),
        thinking_level,
        tool_names: input.tool_names.clone(),
    })
    .await
    {
        Ok(worker) => worker,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Failed to start Pi RPC worker: {error:#}")),
    };
    let session = insert_rpc_worker(&state, Arc::new(worker)).await;
    let initial_state = session.rpc_worker.state().await;
    let state_value = rpc_status_json(&session.ui, &initial_state, true);
    let mut command = input.command;
    for key in ["provider", "modelId", "thinkingLevel", "toolNames"] {
        command.remove(key);
    }
    if !command.contains_key("type") {
        command.insert("type".into(), json!("ensure_session"));
    }
    let command = Value::Object(command.into_iter().collect());
    let result = if command.get("type").and_then(Value::as_str) == Some("ensure_session") {
        Ok(Value::Null)
    } else {
        dispatch_rpc(&session, command).await
    };
    match result {
        Ok(data) => {
            let session_id = data.get("sessionId").and_then(Value::as_str).unwrap_or(&initial_state.session_id);
            (
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "sessionId": session_id,
                    "data": data,
                    "model": initial_state.model.map(|model| json!({"provider": model.provider, "modelId": model.id})),
                    "thinkingLevel": initial_state.thinking_level,
                    "state": state_value,
                })),
            )
                .into_response()
        }
        Err(error) => failure(StatusCode::BAD_GATEWAY, error),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateSessionRequest {
    cwd: String,
    #[serde(flatten)]
    command: HashMap<String, Value>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    thinking_level: Option<String>,
    #[serde(default)]
    tool_names: Option<Vec<String>>,
}

pub(super) async fn status(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Response {
    match get_or_open(&state, &id).await {
        Ok(session) => {
            let include_prompt = request.uri().query().is_none_or(|query| !query.contains("systemPrompt=0"));
            let worker = &session.rpc_worker;
            let rpc_state = match worker.refresh_state().await {
                Ok(state) => state,
                Err(error) => {
                    return failure(StatusCode::BAD_GATEWAY, format!("Could not read RPC session state: {error:#}"));
                }
            };
            (
                StatusCode::OK,
                Json(json!({"running":true, "state":rpc_status_json(&session.ui, &rpc_state, include_prompt)})),
            )
                .into_response()
        }
        Err(error) if error.to_string() == "Session not found" => failure(StatusCode::NOT_FOUND, "Session not found"),
        Err(error) => failure(StatusCode::BAD_GATEWAY, format!("RPC session unavailable: {error:#}")),
    }
}

pub(super) async fn session_state_route(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Response {
    let active = lock(&state.sessions).contains_key(&id);
    let persisted = list_all_sessions(None, &state.agent_dir).iter().any(|session| session.id == id);
    if !active && !persisted {
        return failure(StatusCode::NOT_FOUND, "Session not found");
    }
    status(State(state), RoutePath(id), request).await
}

pub(super) async fn rename_session(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<Value>,
) -> Response {
    let Some(name) = input.get("name").and_then(Value::as_str) else {
        return failure(StatusCode::BAD_REQUEST, "name is required");
    };
    let name = name.trim();
    let session = match get_or_open(&state, &id).await {
        Ok(session) => session,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    match dispatch_rpc(&session, json!({"type":"set_session_name", "name":name})).await {
        Ok(_) => ok(json!({"ok":true})),
        Err(error) => failure(StatusCode::BAD_GATEWAY, error),
    }
}

#[derive(Deserialize, Default)]
pub(super) struct ForkSessionRequest {
    #[serde(default)]
    side: bool,
    name: Option<String>,
}

async fn fork_session_via_rpc(state: &AppState, id: &str, source_info: SessionInfo) -> Response {
    let session = match get_or_open(state, id).await {
        Ok(session) => session,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not open session RPC worker: {error:#}")),
    };
    let worker = &session.rpc_worker;
    let fork_messages = match worker.request(RpcCommand::GetForkMessages).await {
        Ok(value) => value["messages"].as_array().cloned().unwrap_or_default(),
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not read forkable messages: {error:#}")),
    };
    if fork_messages.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Nothing to fork: the session has no messages","code":"empty"})),
        )
            .into_response();
    }

    let mut rng = [0_u8; 2];
    if let Err(error) = SystemRandom::new().fill(&mut rng) {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not create fork name: {error:?}"));
    }
    let suffix = format!("{:02x}{:02x}", rng[0], rng[1]);
    let title =
        source_info.name.as_deref().filter(|name| !name.trim().is_empty()).unwrap_or(&source_info.first_message);
    let title = if title == "(no messages)" || title.trim().is_empty() { &id[..id.len().min(12)] } else { title };
    let mut base = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if source_info.parent_session_path.is_some() {
        if let Some((head, _)) = base.rsplit_once(" 路 ") {
            if base.len() - head.len() == 8 && base[base.len() - 4..].chars().all(|ch| ch.is_ascii_hexdigit()) {
                base = head.trim_end().to_string();
            }
        }
    }
    let chars = base.chars().collect::<Vec<_>>();
    if chars.len() > 40 {
        base = chars[..39].iter().collect::<String>() + "…";
    }
    let name = if base.trim().is_empty() { suffix } else { format!("{base} · {suffix}") };

    let result = match worker.request(RpcCommand::ForkBranch { entry_id: None, before: false }).await {
        Ok(result) => result,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("RPC clone failed: {error:#}")),
    };
    if result["cancelled"] == true {
        return (StatusCode::CONFLICT, Json(json!({"error":"Fork was cancelled","code":"cancelled"}))).into_response();
    }
    let Some(fork_id) = result["newSessionId"].as_str().map(str::to_owned) else {
        return failure(StatusCode::BAD_GATEWAY, "RPC fork did not return a session ID");
    };
    let Some(path) = result["path"].as_str().map(PathBuf::from) else {
        return failure(StatusCode::BAD_GATEWAY, "RPC fork did not return a session file");
    };
    if let Err(error) = SessionManager::open(&path).and_then(|mut branch| branch.append_session_info(&name).map(drop)) {
        return failure(StatusCode::BAD_GATEWAY, format!("Could not name fork: {error:#}"));
    }
    let fork_info = list_all_sessions(None, &state.agent_dir).into_iter().find(|info| info.id == fork_id);
    let (cwd, created, modified, message_count, first_message) = fork_info.map_or_else(
        || (source_info.cwd.clone(), 0, 0, 0, "(no messages)".to_string()),
        |info| (info.cwd, info.created, info.modified, info.message_count, info.first_message),
    );
    ok(json!({
        "sessionId":fork_id,
        "session":{"id":fork_id,"path":path,"cwd":cwd,"name":name,
            "created":pi_coding_agent::session::format_iso(created),"modified":pi_coding_agent::session::format_iso(modified),
            "messageCount":message_count,"firstMessage":first_message,"parentSessionId":id,
            "relation":{"kind":"fork","originSessionId":id},"transient":false}
    }))
}

pub(super) async fn fork_session_file(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Json(input): Json<ForkSessionRequest>,
) -> Response {
    use pi_coding_agent::session::EntryData;
    let refusal =
        |status, code: &str, error: String| (status, Json(json!({"error":error,"code":code}))).into_response();
    let source_info = list_all_sessions(None, &state.agent_dir).into_iter().find(|info| info.id == id);
    let Some(source_info) = source_info else {
        return refusal(StatusCode::NOT_FOUND, "not_found", "Session not found".into());
    };
    if !input.side {
        return fork_session_via_rpc(&state, &id, source_info).await;
    }
    let active = { lock(&state.sessions).get(&id).cloned() };
    if let Some(active) = &active {
        let busy = match active.is_busy().await {
            Ok(busy) => busy,
            Err(error) => {
                return refusal(
                    StatusCode::BAD_GATEWAY,
                    "rpc_error",
                    format!("Could not query session state: {error:#}"),
                );
            }
        };
        if busy {
            return refusal(
                StatusCode::CONFLICT,
                "running",
                "Wait for the RPC session to finish before forking it".into(),
            );
        }
    }
    let mut manager = match SessionManager::open(&source_info.path) {
        Ok(manager) => manager,
        Err(error) => return refusal(StatusCode::NOT_FOUND, "not_found", format!("Session not found: {error:#}")),
    };
    let Some(leaf_id) = manager.leaf_id().map(str::to_string) else {
        return refusal(StatusCode::CONFLICT, "empty", "Nothing to fork: the session has no messages".into());
    };
    let branch = manager.get_branch(Some(&leaf_id));
    if branch.iter().any(|entry| matches!(entry.data(), EntryData::CustomMessage(message) if message.custom_type == "pi-web:side-boundary"))
        || branch.iter().any(|entry| matches!(entry.data(), EntryData::Custom { custom_type, .. } if custom_type == "pi-web:side"))
    {
        return refusal(StatusCode::CONFLICT, "side", "A side chat cannot be forked".into());
    }
    if !branch.iter().any(|entry| entry.kind() == "message") {
        return refusal(StatusCode::CONFLICT, "empty", "Nothing to fork: the session has no messages".into());
    }
    let mut rng = [0_u8; 2];
    if let Err(error) = SystemRandom::new().fill(&mut rng) {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not create fork name: {error:?}"));
    }
    let suffix = format!("{:02x}{:02x}", rng[0], rng[1]);
    let title =
        source_info.name.as_deref().filter(|name| !name.trim().is_empty()).unwrap_or(&source_info.first_message);
    let title = if title == "(no messages)" || title.trim().is_empty() { &id[..id.len().min(12)] } else { title };
    let mut base = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if !input.side && source_info.parent_session_path.is_some() {
        if let Some((head, _)) = base.rsplit_once(" · ") {
            if base.len() - head.len() == 8 && base[base.len() - 4..].chars().all(|ch| ch.is_ascii_hexdigit()) {
                base = head.trim_end().to_string();
            }
        }
    }
    let chars = base.chars().collect::<Vec<_>>();
    if chars.len() > 40 {
        base = chars[..39].iter().collect::<String>() + "…";
    }
    let name = if input.side {
        input
            .name
            .as_deref()
            .and_then(|name| name.lines().next())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| {
                let chars = name.chars().collect::<Vec<_>>();
                if chars.len() > 40 { chars[..39].iter().collect::<String>() + "…" } else { name.to_string() }
            })
            .unwrap_or_else(|| "Side chat".to_string())
    } else if base.trim().is_empty() {
        suffix.clone()
    } else {
        format!("{base} · {suffix}")
    };
    let path = match manager.create_branched_session(&leaf_id) {
        Ok(Some(path)) => path,
        Ok(None) => return refusal(StatusCode::CONFLICT, "unsaved", "Session could not be persisted".into()),
        Err(error) => return refusal(StatusCode::CONFLICT, "failed", format!("{error:#}")),
    };
    if let Err(error) = manager.append_session_info(&name) {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not name fork: {error:#}"));
    }
    let fork_id = manager.session_id().to_string();
    if input.side {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read side chat copy: {error}"));
            }
        };
        let mut lines = contents.lines().map(str::to_string).collect::<Vec<_>>();
        if lines.len() < 2 {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, "Side chat copy is empty");
        }
        let Ok(mut first_entry) = serde_json::from_str::<Value>(&lines[1]) else {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, "Side chat copy has an invalid first entry");
        };
        let now = pi_coding_agent::session::iso_timestamp();
        let side_meta_id = format!("side-{}", pi_coding_agent::session::uuid_v7());
        let first_entry_id = first_entry["id"].as_str().unwrap_or_default().to_string();
        first_entry["parentId"] = json!(side_meta_id);
        lines[1] = first_entry.to_string();
        let metadata = json!({
            "type":"custom","customType":"pi-web:side",
            "data":{"version":1,"parentSessionId":id,"createdAt":now},
            "id":side_meta_id,"parentId":null,"timestamp":now
        });
        lines.insert(1, metadata.to_string());
        let allowed = ["read", "bash", "powershell", "grep", "find", "ls"];
        let parent_entries =
            manager.entries().iter().map(|entry| Value::Object(entry.raw().clone())).collect::<Vec<_>>();
        let parent_tools =
            session_tool_selection(&parent_entries).unwrap_or_else(|| vec!["read".into(), "bash".into()]);
        let tools = parent_tools.into_iter().filter(|tool| allowed.contains(&tool.as_str())).collect::<Vec<_>>();
        let mut tail_parent = manager.leaf_id().map(str::to_string).unwrap_or(first_entry_id);
        let pin_id = format!("side-tools-{}", pi_coding_agent::session::uuid_v7());
        let pin = json!({
            "type":"custom","customType":"pi-web:tool-selection",
            "data":{"version":1,"tools":tools},"id":pin_id,"parentId":tail_parent,"timestamp":now
        });
        tail_parent = pin_id;
        lines.push(pin.to_string());
        let boundary = json!({
            "type":"custom_message","customType":"pi-web:side-boundary",
            "content":"[Side chat] This is a note from the app, not from the user. Everything above is inherited from a parent session as background, frozen when this side chat started. It is not your current task, and work left unfinished in it is not yours to continue. Only messages after this note are active.",
            "display":false,"id":format!("side-boundary-{}",pi_coding_agent::session::uuid_v7()),
            "parentId":tail_parent,"timestamp":now
        });
        lines.push(boundary.to_string());
        let mut output = lines.join("\n");
        if contents.ends_with('\n') {
            output.push('\n');
        }
        if let Err(error) = fs::write(&path, output) {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not write side chat metadata: {error}"));
        }
    }
    let cwd = if source_info.cwd.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        PathBuf::from(&source_info.cwd)
    };
    match RpcSessionWorker::open_session_file(&path, cwd, state.agent_dir.clone()).await {
        Ok(worker) => {
            insert_rpc_worker(&state, Arc::new(worker)).await;
        }
        Err(error) => {
            let _ = pi_coding_agent::session::listing::delete_session_file(&path);
            return failure(StatusCode::BAD_GATEWAY, format!("Could not start forked RPC session: {error:#}"));
        }
    }
    let fork_info = list_all_sessions(None, &state.agent_dir).into_iter().find(|info| info.id == fork_id);
    let (cwd, created, modified, message_count, first_message) = fork_info.map_or_else(
        || (source_info.cwd.clone(), 0, 0, 0, "(no messages)".to_string()),
        |info| (info.cwd, info.created, info.modified, info.message_count, info.first_message),
    );
    ok(json!({
        "sessionId":fork_id,
        "session":{
            "id":fork_id,"path":path,"cwd":cwd,"name":name,
            "created":pi_coding_agent::session::format_iso(created),
            "modified":pi_coding_agent::session::format_iso(modified),
            "messageCount":message_count,"firstMessage":first_message,
            "parentSessionId":id,
            "relation":if input.side { json!({"kind":"side","parentSessionId":id}) } else { json!({"kind":"fork","originSessionId":id}) },
            "transient":false
        }
    }))
}

fn normalized_session_path(path: &std::path::Path) -> String {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let value = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { value.to_lowercase() } else { value }
}

fn read_session_header(path: &std::path::Path) -> Option<Value> {
    use std::io::BufRead;
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    let value = serde_json::from_str::<Value>(&line).ok()?;
    (value["type"] == "session").then_some(value)
}

fn read_session_family_parent(path: &std::path::Path) -> Option<String> {
    use std::io::BufRead;
    let file = fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines().take(96).skip(1).flatten() {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
        if entry["customType"] == "pi-web:side" {
            if let Some(parent) = entry["data"]["parentSessionId"].as_str() {
                return Some(parent.to_string());
            }
        }
    }
    None
}

pub(super) async fn delete_session(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    let catalog = list_all_sessions(None, &state.agent_dir);
    let mut paths = catalog.iter().map(|info| (info.id.clone(), info.path.clone())).collect::<HashMap<_, _>>();
    let active = { lock(&state.sessions).get(&id).cloned() };
    if let Some(active) = active {
        if let Some(path) = active.session_file().await {
            paths.insert(id.clone(), path);
        }
    }
    if !paths.contains_key(&id) {
        return failure(StatusCode::NOT_FOUND, "Session not found");
    }

    let mut children = HashMap::<String, Vec<String>>::new();
    for (child_id, path) in &paths {
        if let Some(parent_id) = read_session_family_parent(path) {
            children.entry(parent_id).or_default().push(child_id.clone());
        }
    }
    fn visit(id: &str, children: &HashMap<String, Vec<String>>, seen: &mut HashSet<String>, ordered: &mut Vec<String>) {
        if !seen.insert(id.to_string()) {
            return;
        }
        for child in children.get(id).into_iter().flatten() {
            visit(child, children, seen, ordered);
        }
        ordered.push(id.to_string());
    }
    let mut deleted_ids = Vec::new();
    visit(&id, &children, &mut HashSet::new(), &mut deleted_ids);
    let deleted_paths = deleted_ids
        .iter()
        .filter_map(|id| paths.get(id).map(|path| (id.clone(), path.clone())))
        .collect::<HashMap<_, _>>();
    let deleted_keys = deleted_paths.values().map(|path| normalized_session_path(path)).collect::<HashSet<_>>();
    let parents = deleted_paths
        .values()
        .map(|path| {
            let parent =
                read_session_header(path).and_then(|header| header["parentSession"].as_str().map(str::to_string));
            (normalized_session_path(path), parent)
        })
        .collect::<HashMap<_, _>>();

    // If a surviving fork names a deleted parent, reconnect it to the nearest surviving ancestor.
    for info in &catalog {
        if deleted_keys.contains(&normalized_session_path(&info.path)) {
            continue;
        }
        let Some(mut header) = read_session_header(&info.path) else { continue };
        let Some(original_parent) = header["parentSession"].as_str().map(str::to_string) else { continue };
        if !deleted_keys.contains(&normalized_session_path(std::path::Path::new(&original_parent))) {
            continue;
        }
        let mut candidate = Some(original_parent);
        let mut seen = HashSet::new();
        let mut surviving = None;
        while let Some(path) = candidate.take() {
            let key = normalized_session_path(std::path::Path::new(&path));
            if !seen.insert(key.clone()) {
                break;
            }
            if !deleted_keys.contains(&key) {
                surviving = Some(path);
                break;
            }
            candidate = parents.get(&key).cloned().flatten();
        }
        header["parentSession"] = surviving.as_ref().map_or(Value::Null, |path| json!(path));
        let Ok(contents) = fs::read_to_string(&info.path) else { continue };
        let mut lines = contents.lines().map(str::to_string).collect::<Vec<_>>();
        if let Some(first) = lines.first_mut() {
            *first = header.to_string();
        }
        let parent_id = surviving.as_ref().and_then(|path| {
            catalog
                .iter()
                .find(|candidate| {
                    normalized_session_path(&candidate.path) == normalized_session_path(std::path::Path::new(path))
                })
                .map(|candidate| candidate.id.as_str())
        });
        let mut index = 1;
        while index < lines.len() {
            let Ok(mut entry) = serde_json::from_str::<Value>(&lines[index]) else {
                index += 1;
                continue;
            };
            let kind = entry["customType"].as_str().map(str::to_string);
            if kind.as_deref() == Some("pi-web:side") {
                if let (Some(parent_id), Some(_)) = (parent_id, surviving.as_deref()) {
                    entry["data"]["parentSessionId"] = json!(parent_id);
                    lines[index] = entry.to_string();
                } else {
                    lines.remove(index);
                    continue;
                }
            }
            index += 1;
        }
        let mut output = lines.join("\n");
        if contents.ends_with('\n') {
            output.push('\n');
        }
        if let Err(error) = fs::write(&info.path, output) {
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Could not update child session metadata: {error}"),
            );
        }
    }

    for session_id in &deleted_ids {
        let runtime = { lock(&state.sessions).remove(session_id) };
        if let Some(runtime) = runtime {
            runtime.dispose().await;
        }
    }
    let mut deleted = Vec::new();
    for session_id in deleted_ids {
        if let Some(path) = deleted_paths.get(&session_id) {
            if let Err(error) = pi_coding_agent::session::listing::delete_session_file(path) {
                return failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Could not delete session {session_id}: {error:#}"),
                );
            }
        }
        deleted.push(session_id);
    }
    session_ui_state::forget(&state.agent_dir, &deleted);
    ok(json!({"deleted":deleted,"missing":[],"running":[]}))
}

#[derive(Deserialize)]
pub(super) struct DeleteSessionsRequest {
    ids: Vec<String>,
}

pub(super) async fn delete_sessions(
    State(state): State<AppState>,
    Json(input): Json<DeleteSessionsRequest>,
) -> Response {
    if input.ids.is_empty() {
        return failure(StatusCode::BAD_REQUEST, "ids must be a non-empty array");
    }
    if input.ids.len() > 500 {
        return failure(StatusCode::BAD_REQUEST, "ids may hold at most 500 session ids");
    }
    if input.ids.iter().any(|id| {
        id.is_empty() || id.len() > 128 || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || "_-".contains(ch))
    }) {
        return failure(StatusCode::BAD_REQUEST, "ids must be session ids");
    }

    let catalog = list_all_sessions(None, &state.agent_dir);
    let mut paths = catalog.iter().map(|info| (info.id.clone(), info.path.clone())).collect::<HashMap<_, _>>();
    let active_sessions =
        lock(&state.sessions).iter().map(|(id, session)| (id.clone(), session.clone())).collect::<Vec<_>>();
    let mut running_ids = HashSet::new();
    for (id, session) in &active_sessions {
        match session.is_busy().await {
            Ok(true) => {
                running_ids.insert(id.clone());
            }
            Ok(false) => {}
            Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not query session {id}: {error:#}")),
        }
    }
    for id in &input.ids {
        let active = { lock(&state.sessions).get(id).cloned() };
        if let Some(active) = active {
            if let Some(path) = active.session_file().await {
                paths.insert(id.clone(), path);
            }
        }
    }
    let mut children = HashMap::<String, Vec<String>>::new();
    for (child_id, path) in &paths {
        if let Some(parent_id) = read_session_family_parent(path) {
            children.entry(parent_id).or_default().push(child_id.clone());
        }
    }
    fn visit(id: &str, children: &HashMap<String, Vec<String>>, seen: &mut HashSet<String>, ordered: &mut Vec<String>) {
        if !seen.insert(id.to_string()) {
            return;
        }
        for child in children.get(id).into_iter().flatten() {
            visit(child, children, seen, ordered);
        }
        ordered.push(id.to_string());
    }
    let mut missing = Vec::new();
    let mut running = Vec::new();
    let mut deleted_ids = Vec::new();
    let mut visited = HashSet::new();
    for id in input.ids {
        if !visited.insert(id.clone()) {
            continue;
        }
        if !paths.contains_key(&id) {
            missing.push(id);
            continue;
        }
        let mut family = Vec::new();
        visit(&id, &children, &mut HashSet::new(), &mut family);
        if family.iter().any(|session_id| running_ids.contains(session_id)) {
            running.push(id);
            continue;
        }
        for session_id in family {
            if visited.insert(session_id.clone()) {
                deleted_ids.push(session_id);
            }
        }
    }
    let mut deleted = Vec::new();
    for session_id in deleted_ids {
        let runtime = { lock(&state.sessions).remove(&session_id) };
        if let Some(runtime) = runtime {
            runtime.dispose().await;
        }
        if let Some(path) = paths.get(&session_id) {
            if let Err(error) = pi_coding_agent::session::listing::delete_session_file(path) {
                return failure(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Could not delete session {session_id}: {error:#}"),
                );
            }
        }
        deleted.push(session_id);
    }
    session_ui_state::forget(&state.agent_dir, &deleted);
    ok(json!({"deleted":deleted,"missing":missing,"running":running}))
}

pub(crate) async fn get_session_entries(state: &AppState, id: &str) -> anyhow::Result<(Vec<Value>, Option<String>)> {
    let session = get_or_open(state, id).await?;
    let response = session.rpc_worker.request(RpcCommand::GetEntries { since: None }).await?;
    let entries = response["entries"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("RPC get_entries response has no entries"))?
        .clone();
    Ok((entries, response["leafId"].as_str().map(str::to_string)))
}

pub(super) async fn session_entry_thinking(
    State(state): State<AppState>,
    RoutePath((id, entry_id)): RoutePath<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(block_index) = query.get("blockIndex").and_then(|value| value.parse::<usize>().ok()) else {
        return failure(StatusCode::BAD_REQUEST, "Valid blockIndex is required");
    };
    let (entries, _) = match get_session_entries(&state, &id).await {
        Ok(entries) => entries,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let Some(raw) = entries.iter().find(|entry| entry["id"] == entry_id) else {
        return failure(StatusCode::NOT_FOUND, "Assistant message not found");
    };
    if raw["type"] != "message" || raw["message"]["role"] != "assistant" {
        return failure(StatusCode::NOT_FOUND, "Assistant message not found");
    }
    let Some(block) = raw["message"]["content"].as_array().and_then(|blocks| blocks.get(block_index)) else {
        return failure(StatusCode::NOT_FOUND, "Thinking block not found");
    };
    if block["type"] != "thinking" {
        return failure(StatusCode::NOT_FOUND, "Thinking block not found");
    }
    Json(json!({"thinking":block["thinking"].as_str().unwrap_or_default()})).into_response()
}

pub(super) async fn session_entry_tool_result_image(
    State(state): State<AppState>,
    RoutePath((id, entry_id)): RoutePath<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(block_index) = query.get("blockIndex").and_then(|value| value.parse::<usize>().ok()) else {
        return failure(StatusCode::BAD_REQUEST, "Valid blockIndex is required");
    };
    let (entries, _) = match get_session_entries(&state, &id).await {
        Ok(entries) => entries,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let Some(raw) = entries.iter().find(|entry| entry["id"] == entry_id) else {
        return failure(StatusCode::NOT_FOUND, "Tool result not found");
    };
    if raw["type"] != "message" || raw["message"]["role"] != "toolResult" {
        return failure(StatusCode::NOT_FOUND, "Tool result not found");
    }
    let Some(block) = raw["message"]["content"].as_array().and_then(|blocks| blocks.get(block_index)) else {
        return failure(StatusCode::NOT_FOUND, "Tool result image not found");
    };
    if block["type"] != "image" {
        return failure(StatusCode::NOT_FOUND, "Tool result image not found");
    }
    let image = if block["data"].is_string() {
        Some((block["data"].as_str().unwrap_or_default(), block["mimeType"].as_str().unwrap_or_default()))
    } else if block["source"]["type"] == "base64" {
        Some((
            block["source"]["data"].as_str().unwrap_or_default(),
            block["source"]["media_type"].as_str().unwrap_or_default(),
        ))
    } else {
        None
    };
    let Some((encoded, mime)) = image else {
        return failure(StatusCode::NOT_FOUND, "Tool result image not found");
    };
    if !matches!(mime, "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "image/bmp" | "image/avif") {
        return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported image type");
    }
    if encoded.is_empty() || encoded.len() > 13_981_016 {
        return failure(StatusCode::PAYLOAD_TOO_LARGE, "Invalid or oversized image data");
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return failure(StatusCode::PAYLOAD_TOO_LARGE, "Invalid or oversized image data");
    };
    if bytes.is_empty() || bytes.len() > 10 * 1024 * 1024 {
        return failure(StatusCode::PAYLOAD_TOO_LARGE, "Invalid or oversized image data");
    }
    let mut response = Response::new(axum::body::Body::from(bytes.clone()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap());
    response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from_str(&bytes.len().to_string()).unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    response.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    response
}

pub(super) async fn export_session_html(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(info) = list_all_sessions(None, &state.agent_dir).into_iter().find(|info| info.id == id) else {
        return failure(StatusCode::NOT_FOUND, "Session not found");
    };
    let session = match get_or_open(&state, &id).await {
        Ok(session) => session,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not open session RPC worker: {error:#}")),
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let output = std::env::temp_dir().join(format!("pi-web-export-{}-{stamp}.html", std::process::id()));
    if let Err(error) = session
        .rpc_worker
        .request(RpcCommand::ExportHtml { output_path: Some(output.to_string_lossy().into_owned()) })
        .await
    {
        return failure(StatusCode::BAD_GATEWAY, format!("RPC export failed: {error:#}"));
    }
    let html = match tokio::fs::read_to_string(&output).await {
        Ok(html) => html,
        Err(error) => {
            let _ = tokio::fs::remove_file(&output).await;
            return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read exported HTML: {error}"));
        }
    };
    let _ = tokio::fs::remove_file(&output).await;
    let stem = info.path.file_stem().and_then(|name| name.to_str()).unwrap_or("session");
    let file_name = format!("pi-session-{stem}.html");
    let disposition = if query.get("inline").is_some_and(|value| value == "1") { "inline" } else { "attachment" };
    let mut response = Response::new(axum::body::Body::from(html));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("{disposition}; filename=\"{file_name}\"; filename*=UTF-8''{file_name}"))
            .unwrap(),
    );
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("frame-ancestors 'none'"));
    response.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    response.headers_mut().insert("x-frame-options", HeaderValue::from_static("DENY"));
    response
}

#[derive(Deserialize)]
pub(super) struct SessionStatsRequest {
    ids: Vec<String>,
}

fn session_usage_summary(entries: &[Value], rpc_stats: &Value) -> Value {
    let side_created_at = entries.iter().find_map(|entry| {
        (entry["type"] == "custom" && entry["customType"] == "pi-web:side")
            .then(|| entry["data"]["createdAt"].as_str())
            .flatten()
    });
    let entries = entries
        .iter()
        .filter(|entry| side_created_at.is_none_or(|created| entry["timestamp"].as_str().unwrap_or_default() > created))
        .collect::<Vec<_>>();
    let token_totals = rpc_stats.get("tokens").cloned().unwrap_or_else(|| json!({}));
    let tool_calls = rpc_stats["toolCalls"].as_u64().unwrap_or(0);
    let mut active_ms = 0_i64;
    let mut previous = None;
    for entry in &entries {
        if !matches!(entry["type"].as_str(), Some("message" | "compaction" | "branch_summary" | "custom_message")) {
            continue;
        }
        let Some(timestamp) = entry["timestamp"].as_str().and_then(pi_coding_agent::session::parse_iso) else {
            continue;
        };
        let role = entry["message"]["role"].as_str().unwrap_or_default();
        if role == "user" || role == "bashExecution" {
            previous = Some(timestamp);
            continue;
        }
        if let Some(last) = previous.filter(|last| timestamp > *last) {
            active_ms += timestamp - last;
        }
        previous = Some(timestamp);
    }
    let mut model: Option<Value> = None;
    let mut thinking_level = "off".to_string();
    for entry in &entries {
        match entry["type"].as_str() {
            Some("model_change") => model = Some(json!({"provider":entry["provider"],"modelId":entry["modelId"]})),
            Some("thinking_level_change") => {
                thinking_level = entry["thinkingLevel"].as_str().unwrap_or("off").to_string()
            }
            Some("message") if entry["message"]["role"] == "assistant" => {
                model = Some(json!({"provider":entry["message"]["provider"],"modelId":entry["message"]["model"]}));
            }
            _ => {}
        }
    }
    let mut summary = json!({
        "tokens":token_totals,
        "cost":rpc_stats["cost"],
        "toolCalls":tool_calls,
        "totalActiveMs":active_ms,
        "model":model,
        "thinkingLevel":thinking_level,
    });
    // The RPC worker owns the canonical totals. Side chats intentionally exclude
    // their inherited transcript, so retain the web-specific filtered totals there.
    if side_created_at.is_some() {
        summary["toolCalls"] = json!(tool_calls);
    }
    summary
}

pub(super) async fn session_stats(State(state): State<AppState>, Json(input): Json<SessionStatsRequest>) -> Response {
    if input.ids.is_empty() || input.ids.len() > 200 {
        return failure(StatusCode::BAD_REQUEST, "ids must contain 1 to 200 session ids");
    }
    if input.ids.iter().any(|id| {
        id.is_empty() || id.len() > 128 || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || "_-".contains(ch))
    }) {
        return failure(StatusCode::BAD_REQUEST, "ids must be session ids");
    }
    let mut stats = serde_json::Map::new();
    for id in input.ids {
        if stats.contains_key(&id) {
            continue;
        }
        if let Ok((entries, _)) = get_session_entries(&state, &id).await {
            if let Ok(session) = get_or_open(&state, &id).await {
                if let Ok(rpc_stats) = session.rpc_worker.request(RpcCommand::GetSessionStats).await {
                    stats.insert(id, session_usage_summary(&entries, &rpc_stats));
                }
            }
        }
    }
    ok(json!({"stats":stats}))
}

pub(super) async fn session_context(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (entries, leaf_id) = match get_session_entries(&state, &id).await {
        Ok(entries) => entries,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let requested_tail = query.get("tail").and_then(|value| value.parse::<i64>().ok()).unwrap_or(50);
    let tail = if requested_tail > 0 { requested_tail.min(1000) as usize } else { 50 };
    let response = session_context::build(
        &entries,
        leaf_id.as_deref(),
        &id,
        query.get("leafId").map(String::as_str),
        query.get("before").map(String::as_str),
        tail,
        query.contains_key("deferThinking"),
        query.contains_key("deferMedia"),
    );
    Json(response).into_response()
}

pub(super) async fn session_file_reference_route(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(path) = query.get("path") else { return failure(StatusCode::BAD_REQUEST, "path is required") };
    let (entries, _) = match get_session_entries(&state, &id).await {
        Ok(entries) => entries,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let referenced = if query.get("kind").is_some_and(|kind| kind == "bash") {
        session_file_reference::is_bash_output_path_referenced(&entries, path)
    } else {
        session_file_reference::is_file_path_referenced(&entries, path)
    };
    Json(json!({"referenced":referenced})).into_response()
}

fn session_project_key(path: &str) -> String {
    let mut key = path.replace('\\', "/");
    if Path::new(path).parent().is_some() {
        key = key.trim_end_matches('/').to_string();
    }
    if cfg!(windows) {
        key = key.to_lowercase();
    }
    key
}

#[derive(Default)]
struct RawUsageTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
}

fn raw_usage_totals(entries: &[&Value]) -> RawUsageTotals {
    let mut totals = RawUsageTotals::default();
    for entry in entries {
        let usage = match entry["type"].as_str() {
            Some("message") if entry["message"]["role"] == "assistant" => &entry["message"]["usage"],
            Some("message") if entry["message"]["role"] == "toolResult" => &entry["message"]["usage"],
            Some("usage" | "compaction" | "branch_summary") => &entry["usage"],
            _ => continue,
        };
        totals.input += usage["input"].as_u64().unwrap_or(0);
        totals.output += usage["output"].as_u64().unwrap_or(0);
        totals.cache_read += usage["cacheRead"].as_u64().unwrap_or(0);
        totals.cache_write += usage["cacheWrite"].as_u64().unwrap_or(0);
        totals.cost += usage["cost"]["total"].as_f64().unwrap_or(0.0);
    }
    totals
}

pub(crate) fn session_project_fields(cwd: &str) -> Value {
    let fallback = || json!({"projectRoot":cwd,"projectKey":session_project_key(cwd)});
    if cwd.is_empty() || !Path::new(cwd).exists() {
        return fallback();
    }
    let Ok(output) = Command::new("git")
        .args([
            "-C",
            cwd,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--git-dir",
            "--show-toplevel",
            "--abbrev-ref",
            "HEAD",
        ])
        .output()
    else {
        return fallback();
    };
    if !output.status.success() {
        return fallback();
    }
    let lines = String::from_utf8_lossy(&output.stdout).lines().map(str::trim).map(str::to_string).collect::<Vec<_>>();
    if lines.len() < 4 {
        return fallback();
    }
    let (common, git_dir, top, branch) =
        (PathBuf::from(&lines[0]), PathBuf::from(&lines[1]), PathBuf::from(&lines[2]), &lines[3]);
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let is_top = canonical(&top) == canonical(Path::new(cwd));
    let is_worktree = is_top && canonical(&git_dir) != canonical(&common);
    let root = if is_top && is_worktree {
        if common.file_name().is_some_and(|name| name == ".git") {
            common.parent().unwrap_or(&common).to_path_buf()
        } else {
            common.clone()
        }
    } else if is_top {
        top
    } else {
        PathBuf::from(cwd)
    };
    let root = root.to_string_lossy().to_string();
    let key = session_project_key(&root);
    let mut value = json!({"projectRoot":root,"projectKey":key});
    if branch != "HEAD" && !branch.is_empty() {
        value["branch"] = json!(branch);
    }
    if is_worktree {
        value["isWorktree"] = json!(true);
    }
    value
}

pub(super) async fn session_detail_snapshot(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if query.get("opened").is_some_and(|value| value == "1")
        && let Some(session) = lock(&state.sessions).get(&id)
    {
        session.rpc_worker.reset_idle_timer();
    }
    let (entries, leaf_id) = match get_session_entries(&state, &id).await {
        Ok(entries) => entries,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let Some(catalog_info) = list_all_sessions(None, &state.agent_dir).into_iter().find(|info| info.id == id) else {
        return failure(StatusCode::NOT_FOUND, "Session not found");
    };
    let session = match get_or_open(&state, &id).await {
        Ok(session) => session,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not open session RPC worker: {error:#}")),
    };
    let tree_data = match session.rpc_worker.request(RpcCommand::GetTree).await {
        Ok(tree) => tree,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not read RPC session tree: {error:#}")),
    };
    let tree = match pi_coding_agent::rpc::RpcSessionTree::from_data(tree_data) {
        Ok(tree) => tree,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Invalid RPC session tree: {error}")),
    };
    let rpc_stats = match session.rpc_worker.request(RpcCommand::GetSessionStats).await {
        Ok(stats) => stats,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not read RPC session stats: {error:#}")),
    };
    let summary_tree = query.get("tree").is_some_and(|value| value == "summary");
    let projected_tree = session_tree_projection::build(&tree, summary_tree);
    let side_created_at = entries.iter().find_map(|entry| {
        (entry["type"] == "custom" && entry["customType"] == "pi-web:side")
            .then(|| entry["data"]["createdAt"].as_str())
            .flatten()
    });
    let own_entries = entries
        .iter()
        .filter(|entry| side_created_at.is_none_or(|created| entry["timestamp"].as_str().unwrap_or_default() > created))
        .collect::<Vec<_>>();
    let usage = raw_usage_totals(&own_entries);
    let mut stats = (0_usize, 0_usize, 0_usize, 0_usize, 0_usize);
    for entry in &own_entries {
        if entry["type"] == "message" {
            stats.4 += 1;
            match entry["message"]["role"].as_str() {
                Some("user") => stats.0 += 1,
                Some("assistant") => {
                    stats.1 += 1;
                    stats.2 += entry["message"]["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|block| block["type"] == "toolCall")
                        .count();
                }
                Some("toolResult") => stats.3 += 1,
                _ => {}
            }
        }
    }
    let (user_messages, assistant_messages, tool_calls, tool_results, total_messages, tokens, cost) = if side_created_at
        .is_none()
    {
        (
            rpc_stats["userMessages"].as_u64().unwrap_or(0) as usize,
            rpc_stats["assistantMessages"].as_u64().unwrap_or(0) as usize,
            rpc_stats["toolCalls"].as_u64().unwrap_or(0) as usize,
            rpc_stats["toolResults"].as_u64().unwrap_or(0) as usize,
            rpc_stats["totalMessages"].as_u64().unwrap_or(0) as usize,
            rpc_stats["tokens"].clone(),
            rpc_stats["cost"].clone(),
        )
    } else {
        (
            stats.0,
            stats.1,
            stats.2,
            stats.3,
            stats.4,
            json!({"input":usage.input,"output":usage.output,"cacheRead":usage.cache_read,"cacheWrite":usage.cache_write,"total":usage.input + usage.output + usage.cache_read + usage.cache_write}),
            json!(usage.cost),
        )
    };
    let file_path = catalog_info.path.to_string_lossy().to_string();
    let metadata = fs::metadata(&catalog_info.path).ok();
    let modified_ms = metadata
        .as_ref()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64);
    let header = entries.first().cloned().unwrap_or_else(|| json!({}));
    let modified = modified_ms
        .map(pi_coding_agent::session::format_iso)
        .unwrap_or_else(|| header["timestamp"].as_str().unwrap_or_default().to_string());
    let latest_entry_id = entries.last().and_then(|entry| entry["id"].as_str());
    use std::hash::{Hash, Hasher};
    let mut revision_hasher = std::collections::hash_map::DefaultHasher::new();
    (metadata.as_ref().map(|meta| meta.len()), modified_ms, entries.len(), latest_entry_id, leaf_id.as_deref())
        .hash(&mut revision_hasher);
    let mut active_ms = 0_i64;
    let mut previous = None;
    for entry in &own_entries {
        if !matches!(entry["type"].as_str(), Some("message" | "compaction" | "branch_summary" | "custom_message")) {
            continue;
        }
        let Some(timestamp) = entry["timestamp"].as_str().and_then(pi_coding_agent::session::parse_iso) else {
            continue;
        };
        let role = entry["message"]["role"].as_str().unwrap_or_default();
        if role == "user" || role == "bashExecution" {
            previous = Some(timestamp);
            continue;
        }
        if let Some(last) = previous.filter(|last| timestamp > *last) {
            active_ms += timestamp - last;
        }
        previous = Some(timestamp);
    }
    let requested_tail = query.get("tail").and_then(|value| value.parse::<i64>().ok()).unwrap_or(50);
    let tail = if requested_tail > 0 { requested_tail.min(1000) as usize } else { 50 };
    let context_page = session_context::build(
        &entries,
        leaf_id.as_deref(),
        &id,
        query.get("leafId").map(String::as_str),
        query.get("before").map(String::as_str),
        tail,
        query.contains_key("deferThinking"),
        query.contains_key("deferMedia"),
    );
    // `session_context::build` returns the paging envelope used by the
    // /context endpoint. The detail endpoint exposes the context payload
    // directly, as SessionData expects, so attach notices and return its
    // inner context instead of nesting it a second time.
    let mut context = context_page.get("context").cloned().unwrap_or(Value::Null);
    session_notices::attach(&entries, leaf_id.as_deref(), &mut context, &catalog_info.cwd, &state.agent_dir);
    let tool_names = session_tool_selection(&entries);
    let first_message = entries
        .iter()
        .find_map(|entry| {
            if entry["type"] != "message" || entry["message"]["role"] != "user" {
                return None;
            }
            let content = &entry["message"]["content"];
            Some(if let Some(text) = content.as_str() {
                text.to_string()
            } else {
                content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|block| block["type"] == "text")
                    .and_then(|block| block["text"].as_str())
                    .unwrap_or("(no messages)")
                    .to_string()
            })
        })
        .unwrap_or_else(|| "(no messages)".to_string());
    let session_id = header["id"].as_str().unwrap_or(&id);
    let cwd = header["cwd"].as_str().unwrap_or(&catalog_info.cwd);
    let mut info = json!({
        "path":file_path,"id":session_id,"cwd":cwd,
        "created":header["timestamp"],"modified":modified,"messageCount":stats.4,
        "firstMessage":first_message,"transient":metadata.is_none()
    });
    if let Some(fields) = session_project_fields(cwd).as_object() {
        info.as_object_mut().unwrap().extend(fields.clone());
    }
    if let Some(name) =
        entries.iter().rev().find(|entry| entry["type"] == "session_info").and_then(|entry| entry["name"].as_str())
    {
        info["name"] = json!(name);
    }
    let side = entries.iter().find(|entry| entry["type"] == "custom" && entry["customType"] == "pi-web:side");
    if let Some(entry) = side {
        let data = &entry["data"];
        let parent_id = data["parentSessionId"].as_str();
        if let Some(parent_id) = parent_id {
            info["parentSessionId"] = json!(parent_id);
        }
        if let Some(parent_id) = parent_id {
            info["relation"] = json!({"kind":"side","parentSessionId":parent_id});
        }
    } else if let Some(parent_path) = header["parentSession"].as_str() {
        let origin = list_all_sessions(None, &state.agent_dir)
            .into_iter()
            .find(|item| item.path == PathBuf::from(parent_path))
            .map(|item| item.id);
        info["relation"] =
            origin.map_or_else(|| json!({"kind":"fork"}), |origin| json!({"kind":"fork","originSessionId":origin}));
    }
    let mut response = json!({
        "sessionId":id, "filePath":file_path,
        "snapshotRevision":format!("{:016x}", revision_hasher.finish()),
        "leafId":leaf_id,
        "tree":projected_tree, "info":info,"context":context,
        "stats":{"userMessages":user_messages,"assistantMessages":assistant_messages,"toolCalls":tool_calls,"toolResults":tool_results,"totalMessages":total_messages,
            "tokens":tokens,"cost":cost},
        "totalActiveMs":active_ms
    });
    if summary_tree {
        response["treeFormat"] = json!("summary");
    }
    if let Some(tool_names) = tool_names {
        response["toolNames"] = json!(tool_names);
    }
    Json(response).into_response()
}

pub(crate) fn same_session_cwd(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf()).to_string_lossy().replace('\\', "/");
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf()).to_string_lossy().replace('\\', "/");
    if cfg!(windows) { left.eq_ignore_ascii_case(&right) } else { left == right }
}

pub(super) async fn session_cwd_state(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(cwd) = query.get("cwd").filter(|cwd| !cwd.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "cwd is required");
    };
    let sessions = lock(&state.sessions).values().cloned().collect::<Vec<_>>();
    let mut busy = false;
    for session in sessions {
        if !same_session_cwd(&session.cwd().await, Path::new(cwd)) {
            continue;
        }
        match session.is_busy().await {
            Ok(true) => {
                busy = true;
                break;
            }
            Ok(false) => {}
            Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not query session state: {error:#}")),
        }
    }
    Json(json!({"busy":busy})).into_response()
}

#[derive(Deserialize)]
pub(super) struct SessionCwdRequest {
    cwd: String,
}

pub(super) async fn dispose_sessions_for_cwd(
    State(state): State<AppState>,
    Json(input): Json<SessionCwdRequest>,
) -> Response {
    if input.cwd.is_empty() {
        return failure(StatusCode::BAD_REQUEST, "cwd is required");
    }
    let candidates =
        lock(&state.sessions).iter().map(|(id, session)| (id.clone(), session.clone())).collect::<Vec<_>>();
    let mut ids = Vec::new();
    for (id, session) in candidates {
        if same_session_cwd(&session.cwd().await, Path::new(&input.cwd)) {
            ids.push(id);
        }
    }
    let disposed = {
        let mut sessions = lock(&state.sessions);
        ids.into_iter().filter_map(|id| sessions.remove(&id)).collect::<Vec<_>>()
    };
    let count = disposed.len();
    for session in disposed {
        session.dispose().await;
    }
    Json(json!({"disposed":count})).into_response()
}

pub(super) async fn auto_name_session(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    let session = match get_or_open(&state, &id).await {
        Ok(session) => session,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let result = match session.rpc_worker.generate_and_set_session_title(&id).await {
        Ok(result) => result,
        Err(error) => {
            let still_same_session = session.rpc_worker.state().await.session_id == id;
            return if still_same_session {
                failure(StatusCode::BAD_GATEWAY, format!("{error:#}"))
            } else {
                failure(StatusCode::CONFLICT, format!("{error:#}"))
            };
        }
    };
    let still_active =
        lock(&state.sessions).get(&id).is_some_and(|current| Arc::ptr_eq(&current.rpc_worker, &session.rpc_worker));
    if !still_active {
        return failure(
            StatusCode::CONFLICT,
            "The session was closed or switched while its title was being generated. Please try again.",
        );
    }
    Json(result).into_response()
}

pub(super) async fn events(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    let session = match get_or_open(&state, &id).await {
        Ok(session) => session,
        Err(error) => return failure(StatusCode::NOT_FOUND, format!("{error:#}")),
    };
    let receiver = session.events.subscribe();
    let pending = session.ui.pending_requests();
    let mut replay = std::collections::VecDeque::from(pending.clone());
    let state_snapshot = match session.rpc_worker.refresh_state().await {
        Ok(state) => state,
        Err(error) => return failure(StatusCode::BAD_GATEWAY, error.to_string()),
    };
    replay.push_front(json!({
        "type":"connected", "isStreaming":state_snapshot.is_streaming,
        "pendingExtensionUiIds":pending.iter().filter_map(|request| request["id"].as_str()).collect::<Vec<_>>(),
    }));
    let events = stream::unfold(
        (receiver, tokio::time::interval(std::time::Duration::from_secs(12)), replay),
        |(mut receiver, mut heartbeat, mut replay)| async move {
            if let Some(value) = replay.pop_front() {
                return Some((Ok(Event::default().data(value.to_string())), (receiver, heartbeat, replay)));
            }
            loop {
                tokio::select! {
                    item = receiver.recv() => match item {
                        Ok(value) => return Some((Ok(Event::default().data(value.to_string())), (receiver, heartbeat, replay))),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                    _ = heartbeat.tick() => return Some((Ok(Event::default().data(json!({"type":"heartbeat"}).to_string())), (receiver, heartbeat, replay))),
                }
            }
        },
    );
    let stop = state.stop.clone();
    let shutdown = stream::once(async move {
        stop.cancelled().await;
        Ok::<Event, std::convert::Infallible>(Event::default().event("server_shutdown").data("{}"))
    });
    Sse::new(stream::select(events, shutdown)).keep_alive(KeepAlive::default()).into_response()
}

pub(super) async fn command(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Json(command): Json<Value>,
) -> Response {
    let session = match get_or_open(&state, &id).await {
        Ok(session) => session,
        Err(error) => return failure(StatusCode::NOT_FOUND, format!("{error:#}")),
    };
    let is_prompt = command.get("type").and_then(Value::as_str) == Some("prompt");
    if command.get("type").and_then(Value::as_str) == Some("switch_session")
        && let Some(session_path) = command.get("sessionPath").and_then(Value::as_str)
        && let Some(target) =
            list_all_sessions(None, &state.agent_dir).into_iter().find(|info| info.path == PathBuf::from(session_path))
        && target.id != id
        && lock(&state.sessions).contains_key(&target.id)
    {
        return failure(StatusCode::CONFLICT, "The target session is already active in this backend");
    }
    let worker = &session.rpc_worker;
    let previous_identity = worker.identity().await;
    match dispatch_rpc(&session, command).await {
        Ok(data) => {
            if let Some(new_id) = data.get("sessionId").and_then(Value::as_str)
                && new_id != id
            {
                let old_session = previous_identity.session_file.clone();
                let old_web_session = if let Some(path) = old_session {
                    match worker.reopen(&path).await {
                        Ok(worker) => Some(wrap_rpc_worker(Arc::new(worker))),
                        Err(error) => {
                            eprintln!("Failed to reopen the source session after RPC session switch: {error:#}");
                            None
                        }
                    }
                } else {
                    None
                };
                let mut sessions = lock(&state.sessions);
                sessions.remove(&id);
                session.rpc_worker.reset_idle_timer();
                sessions.insert(new_id.to_string(), session);
                if let Some(old_web_session) = old_web_session {
                    sessions.insert(id.clone(), old_web_session);
                }
            }
            ok(data)
        }
        Err(error) => {
            let mut body = json!({"error":error});
            if is_prompt {
                body["code"] = json!("prompt_rejected");
                body["accepted"] = json!(false);
            }
            (StatusCode::BAD_GATEWAY, Json(body)).into_response()
        }
    }
}

pub(super) async fn lease(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    let session = lock(&state.sessions).get(&id).cloned();
    match session {
        Some(session) => {
            let state = match session.rpc_worker.refresh_state().await {
                Ok(state) => state,
                Err(error) => {
                    return failure(StatusCode::BAD_GATEWAY, format!("Could not read RPC session state: {error:#}"));
                }
            };
            let running = state.is_streaming || state.is_bash_running || state.is_compacting;
            (
                StatusCode::OK,
                Json(json!({"success":true, "running":running, "awaitingInput":session.ui.awaiting_input(), "subagentsActive":session.ui.has_subagents()})),
            )
                .into_response()
        }
        None => (
            StatusCode::OK,
            Json(json!({"success": true, "running": false, "awaitingInput": false, "subagentsActive": false})),
        )
            .into_response(),
    }
}

async fn dispatch_rpc(web: &WebSession, mut value: Value) -> Result<Value, String> {
    let worker = &web.rpc_worker;
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    let previous_id = worker.state().await.session_id;

    if kind == "extension_ui_response" {
        let id = value.get("id").and_then(Value::as_str).ok_or("id is required")?;
        let response = if value.get("cancelled").and_then(Value::as_bool) == Some(true) {
            pi_coding_agent::rpc::RpcUiResponse::Cancelled
        } else if let Some(confirmed) = value.get("confirmed").and_then(Value::as_bool) {
            pi_coding_agent::rpc::RpcUiResponse::Confirmed(confirmed)
        } else if let Some(value) = value.get("value").and_then(Value::as_str) {
            pi_coding_agent::rpc::RpcUiResponse::Value(value.to_owned())
        } else {
            pi_coding_agent::rpc::RpcUiResponse::Cancelled
        };
        return worker
            .respond_to_ui(id, response)
            .await
            .map(|()| json!({"resolved":true}))
            .map_err(|error| format!("{error:#}"));
    }

    if kind == "abort" {
        let recalled = worker.request(RpcCommand::ClearQueue).await.map_err(|e| e.to_string())?;
        worker.request(RpcCommand::Abort).await.map_err(|e| e.to_string())?;
        return Ok(recalled);
    }
    if kind == "get_state" {
        return Ok(rpc_status_json(&web.ui, &worker.refresh_state().await.map_err(|e| e.to_string())?, true));
    }
    if kind == "bash" {
        value["webOutput"] = json!(true);
    }
    if matches!(kind.as_str(), "extension_ui_input" | "extension_ui_resize") {
        value["dialogId"] = value["id"].take();
    }
    if kind == "reload" {
        value = json!({"type":"prompt", "message":"/reload"});
    }
    let command: RpcCommand = serde_json::from_value(value).map_err(|error| error.to_string())?;
    let branch = match &command {
        RpcCommand::Fork { entry_id } => {
            Some(RpcCommand::ForkBranch { entry_id: Some(entry_id.clone()), before: true })
        }
        RpcCommand::CloneSession { leaf_id } => {
            Some(RpcCommand::ForkBranch { entry_id: leaf_id.clone(), before: false })
        }
        _ => None,
    };
    if let Some(branch) = branch {
        let mut data = worker.request(branch).await.map_err(|error| format!("{error:#}"))?;
        if let Some(fields) = data.as_object_mut() {
            fields.remove("path");
        }
        return Ok(data);
    }
    let mut result = worker.request(command).await.map_err(|error| format!("{error:#}"))?;
    if kind == "prompt" {
        result["queued"] = json!(result["disposition"] == "queued");
    } else if kind == "get_tools" {
        result = result.get("tools").cloned().unwrap_or(Value::Array(Vec::new()));
    } else if kind == "set_tools" {
        result["sessionId"] = json!(worker.state().await.session_id);
        result["recreated"] = json!(false);
    }
    let new_id = worker.state().await.session_id;
    if new_id != previous_id && result.is_object() {
        result["sessionId"] = json!(new_id);
    }
    Ok(result)
}

pub(super) async fn list_sessions(State(state): State<AppState>) -> Response {
    let session_list_version = refresh_session_catalog_version(&state);
    let sessions = list_all_sessions(None, &state.agent_dir);
    let path_to_id = sessions
        .iter()
        .map(|session| (session.path.to_string_lossy().into_owned(), session.id.clone()))
        .collect::<HashMap<_, _>>();
    let mut project_cache = HashMap::new();
    let entries = sessions
        .iter()
        .map(|info| {
            let fields = project_cache.entry(info.cwd.clone()).or_insert_with(|| session_project_fields(&info.cwd));
            session_catalog_json(info, &path_to_id, fields)
        })
        .collect::<Vec<_>>();
    (StatusCode::OK, Json(json!({"sessions": entries, "sessionListVersion":session_list_version}))).into_response()
}

pub(super) fn refresh_session_catalog_version(state: &AppState) -> u64 {
    use std::hash::{Hash, Hasher};
    let root = state.agent_dir.join("sessions");
    let mut files = Vec::new();
    if let Ok(projects) = fs::read_dir(root) {
        for project in projects.flatten() {
            if let Ok(entries) = fs::read_dir(project.path()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|extension| extension == "jsonl") {
                        // Track content updates as well as catalogue membership so
                        // rename/auto-name mutations invalidate the sidebar version.
                        let metadata = match fs::metadata(&path) {
                            Ok(metadata) => metadata,
                            Err(_) => continue,
                        };
                        let modified = metadata
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|duration| duration.as_nanos())
                            .unwrap_or_default();
                        files.push((path.to_string_lossy().into_owned(), metadata.len(), modified));
                    }
                }
            }
        }
    }
    files.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut hasher);
    let signature = hasher.finish();
    let mut catalog = lock(&state.session_catalog_version);
    match catalog.0 {
        Some(previous) if previous != signature => catalog.1 = catalog.1.saturating_add(1),
        None => catalog.0 = Some(signature),
        _ => {}
    }
    catalog.1
}

pub(super) async fn search_sessions(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let query = query.get("q").map(String::as_str).unwrap_or_default().trim();
    if query.encode_utf16().count() > 200 {
        return failure(StatusCode::BAD_REQUEST, "Search query exceeds 200 characters");
    }
    if query.is_empty() {
        return Json(json!({"results":[],"truncated":false})).into_response();
    }
    let sessions = list_all_sessions(None, &state.agent_dir);
    Json(session_search::search(&sessions, query)).into_response()
}

pub(super) async fn running_sessions(State(state): State<AppState>) -> Response {
    let session_list_version = refresh_session_catalog_version(&state);
    let sessions = lock(&state.sessions).iter().map(|(id, session)| (id.clone(), session.clone())).collect::<Vec<_>>();
    let mut running = Vec::new();
    let mut awaiting = Vec::new();
    let mut subagents = Vec::new();
    for (id, session) in sessions {
        match session.is_busy().await {
            Ok(true) => running.push(id.clone()),
            Ok(false) => {}
            Err(error) => return failure(StatusCode::BAD_GATEWAY, format!("Could not query session {id}: {error:#}")),
        }
        if session.ui.awaiting_input() {
            awaiting.push(id.clone());
        }
        if session.ui.has_subagents() {
            subagents.push(id);
        }
    }
    (StatusCode::OK, Json(json!({"sessionListVersion":session_list_version,"runningSessionIds": running, "awaitingInputSessionIds": awaiting, "subagentsActiveSessionIds": subagents,"sessionUiStateRevision":session_ui_state::revision(&state.agent_dir)}))).into_response()
}

pub(super) async fn debug_snapshot(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let now = pi_ai::now_ms();
    let requested = query.get("session").map(String::as_str);
    let sessions = lock(&state.sessions).iter().map(|(id, web)| (id.clone(), web.clone())).collect::<Vec<_>>();
    let mut summaries = Vec::with_capacity(sessions.len());
    let mut detail = None;
    for (id, web) in sessions.iter() {
        let rpc_state = match web.rpc_worker.refresh_state().await {
            Ok(state) => state,
            Err(error) => {
                return failure(StatusCode::BAD_GATEWAY, format!("Could not read RPC session state: {error:#}"));
            }
        };
        let streaming = rpc_state.is_streaming;
        let compacting = rpc_state.is_compacting;
        let idle_elapsed_ms = i64::try_from(web.rpc_worker.last_activity().elapsed().as_millis()).unwrap_or(i64::MAX);
        let last_reset_at = now.saturating_sub(idle_elapsed_ms);
        let idle_timeout_ms = SESSION_IDLE_TIMEOUT.as_secs() as i64 * 1000;
        let snapshot = json!({
            "sessionId":id,"state":"alive","stale":false,"subagent":false,"chatOnly":false,
            "createdAt":web.created_at,"startedAt":web.created_at,"closingAt":null,"extensions":"bound",
            "run":{"running":streaming || compacting,"streaming":streaming,"promptRunning":streaming,"pendingPrompts":rpc_state.pending_message_count,
                "bashRunning":rpc_state.is_bash_running,"compacting":compacting,"retrying":false,"queuedMessages":0,"waitingForMcp":false,
                "runningTools":0,"mutatingCommands":0,"replacement":null},
            "idle":{"timeoutMs":idle_timeout_ms,"nextCheckAt":last_reset_at + idle_timeout_ms,"lastResetAt":last_reset_at,"forcedByStop":false},
            "ui":{"awaitingInput":web.ui.awaiting_input(),"pendingRequests":web.ui.pending_requests().len(),"customUis":0},
            "extensionsState":"rpc"
        });
        if requested == Some(id.as_str()) {
            detail = Some(snapshot.clone());
        }
        summaries.push(snapshot);
    }
    let starting = lock(&state.opening_sessions).keys().cloned().collect::<Vec<_>>();
    drop(sessions);
    let terminals = state.terminals.stats();
    let agent_streams = summaries.iter().map(|entry| entry["eventStreams"].as_u64().unwrap_or_default()).sum::<u64>();
    let terminal_listeners = terminals["listeners"].as_u64().unwrap_or_default();
    let file_watches = 0_u64;
    let pages = terminal::debug_page_streams()
        .into_iter()
        .fold(HashMap::<String, Value>::new(), |mut pages, stream| {
            let key = stream.holder.clone().unwrap_or_default();
            let page = pages.entry(key).or_insert_with(|| {
                json!({"holder":stream.holder,"address":stream.address,"client":stream.client,"agent":0,"terminal":0,"fileWatch":0})
            });
            page["terminal"] = json!(page["terminal"].as_u64().unwrap_or_default().saturating_add(1));
            pages
        })
        .into_values()
        .collect::<Vec<_>>();
    Json(json!({
        "now":now,
        "startedAt":state.started_at,
        "starting":starting,
        "sessions":summaries,
        "terminals":terminals,
        "eventStreams":{"total":agent_streams},
        "sseTotal":agent_streams + terminal_listeners + file_watches,
        "pages":pages,
        "fileWatches":file_watches,
        "session":if requested.is_some() { detail } else { None }
    }))
    .into_response()
}
