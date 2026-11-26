use super::*;

use crate::web_backend::app_state::{AppState, WebSession, failure, lock, ok};

fn status_json(session: &AgentSession, ui: &WebExtensionUi, include_system_prompt: bool, bash_running: bool) -> Value {
    let mut state = serde_json::to_value(session_state(session)).unwrap_or(Value::Null);
    if let Some(usage) = session.context_usage() {
        state["contextUsage"] = json!({
            "tokens": usage.tokens,
            "contextWindow": usage.context_window,
            "percent": usage.percent,
        });
    } else {
        state["contextUsage"] = Value::Null;
    }
    state["isPromptRunning"] = json!(session.is_streaming());
    state["isBashRunning"] = json!(bash_running);
    state["queuedMessages"] = json!({
        "steering": session.steering_messages(),
        "followUp": session.follow_up_messages(),
    });
    state["extensionStatuses"] =
        json!(lock(&ui.statuses).iter().map(|(key, text)| json!({"key":key,"text":text})).collect::<Vec<_>>());
    state["extensionWidgets"] = json!(lock(&ui.widgets).values().cloned().collect::<Vec<_>>());
    state["autoCompactionEnabled"] = json!(session.auto_compaction_enabled());
    if include_system_prompt {
        state["systemPrompt"] = json!(session.system_prompt());
    }
    state
}

fn subscribe_session_events(session: &Arc<AgentSession>, events: broadcast::Sender<Value>) {
    session.subscribe(move |event| {
        if event.is_json_event() {
            let _ = events.send(event.to_json());
        }
    });
}

async fn insert_runtime(state: &AppState, manager: SessionManager) -> anyhow::Result<WebSession> {
    let mut options = state.options.clone();
    if let Some(names) = session_tool_selection(&manager) {
        options.tools = Some(names);
    }
    let runtime = Arc::new(state.host.create_app_runtime(options, manager).await?);
    let (events, _) = broadcast::channel(512);
    let ui = WebExtensionUi::new(events.clone());
    let event_sender = events.clone();
    let ui_for_rebind = ui.clone();
    let runtime_weak = Arc::downgrade(&runtime);
    // The rebind callback only handles future session switches. The runtime
    // already has an initial session, so subscribe it here as well or its agent
    // events never reach the browser SSE channel.
    subscribe_session_events(&runtime.session(), events.clone());
    runtime.set_rebind_session(Some(Arc::new(move |session| {
        subscribe_session_events(session, event_sender.clone());
        let bindings = ExtensionBindings {
            ui: ui_for_rebind.clone(),
            mode: ExtensionMode::Rpc,
            runtime: Some(runtime_weak.clone()),
            ..ExtensionBindings::default()
        };
        let session = session.clone();
        tokio::spawn(async move {
            session.bind_extensions(bindings).await;
        });
    })));
    let bindings = ExtensionBindings {
        ui: ui.clone(),
        mode: ExtensionMode::Rpc,
        runtime: Some(Arc::downgrade(&runtime)),
        ..ExtensionBindings::default()
    };
    let session =
        WebSession { runtime, events, ui, bash_running: Arc::new(AtomicBool::new(false)), created_at: pi_ai::now_ms() };
    lock(&state.sessions).insert(session.session().session_id(), session.clone());
    // Publish the session before startup handlers run. A session_start handler
    // may open a UI dialog; the browser needs the session id to subscribe to
    // events and answer it, so binding must not hold up session creation.
    let session_to_bind = session.session();
    tokio::spawn(async move {
        session_to_bind.bind_extensions(bindings).await;
    });
    Ok(session)
}

fn session_tool_selection(manager: &SessionManager) -> Option<Vec<String>> {
    for entry in manager.entries().iter().rev() {
        let raw = entry.raw();
        if raw.get("customType").and_then(Value::as_str) != Some("pi-web:tool-selection") {
            continue;
        }
        let Some(data) = raw.get("data") else { continue };
        if data.get("version").and_then(Value::as_u64) != Some(1) {
            continue;
        }
        if data.get("cleared").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let Some(tools) = data.get("tools").and_then(Value::as_array) else { continue };
        let allowed = ["bash", "read", "edit", "write", "grep", "find", "ls", "powershell"];
        if tools.iter().all(|tool| tool.as_str().is_some_and(|name| allowed.contains(&name))) {
            let mut result = Vec::new();
            for name in tools.iter().filter_map(Value::as_str) {
                if !result.iter().any(|existing| existing == name) {
                    result.push(name.to_string());
                }
            }
            return Some(result);
        }
    }
    None
}

async fn get_or_open(state: &AppState, id: &str) -> anyhow::Result<WebSession> {
    let existing = { lock(&state.sessions).get(id).cloned() };
    if let Some(session) = existing {
        return Ok(session);
    }
    let opening = {
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
    };
    let _opening = opening.lock().await;
    let existing = { lock(&state.sessions).get(id).cloned() };
    if let Some(session) = existing {
        return Ok(session);
    }
    let info = list_all_sessions(None, &state.agent_dir)
        .into_iter()
        .find(|session| session.id == id)
        .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
    let path = info.path;
    let manager = SessionManager::open(&path)?;
    insert_runtime(state, manager).await
}

pub(super) async fn create_session(State(state): State<AppState>, Json(input): Json<CreateSessionRequest>) -> Response {
    if input.cwd.trim().is_empty() {
        return failure(StatusCode::BAD_REQUEST, "cwd is required");
    }
    let cwd = crate::web_backend::cwd::normalize(&input.cwd);
    if !cwd.is_dir() {
        return failure(StatusCode::BAD_REQUEST, format!("cwd does not exist: {}", cwd.display()));
    }
    let thinking = input
        .thinking_level
        .as_deref()
        .map(|level| serde_json::from_value(json!(level)).map_err(|error| format!("Invalid thinking level: {error}")));
    let thinking = match thinking.transpose() {
        Ok(thinking) => thinking,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error),
    };
    let selected_tools = input.tool_names.clone();
    let options = AppOptions {
        agent_dir: state.agent_dir.clone(),
        provider: input.provider,
        model: input.model_id,
        thinking,
        tools: input.tool_names,
        ..state.options.clone()
    };
    let mut manager =
        match SessionManager::create_with(&cwd, Some(&session_dir(&state.agent_dir, &cwd)), Default::default()) {
            Ok(manager) => manager,
            Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
        };
    if let Some(names) = selected_tools {
        if let Err(error) =
            manager.append_custom_entry("pi-web:tool-selection", Some(json!({"version":1, "tools":names})))
        {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"));
        }
    }
    let options_state = AppState { options, ..state.clone() };
    let session = match insert_runtime(&options_state, manager).await {
        Ok(session) => session,
        Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    };
    let agent = session.session();
    let state_value = status_json(&agent, &session.ui, true, false);
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
        dispatch(&state, &session, command).await
    };
    match result {
        Ok(data) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "sessionId": agent.session_id(),
                "data": data,
                "model": agent.model().map(|model| json!({"provider": model.provider, "modelId": model.id})),
                "thinkingLevel": agent.thinking_level().as_str(),
                "state": state_value,
            })),
        )
            .into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error),
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
            (StatusCode::OK, Json(json!({
                "running": true,
                "state": status_json(&session.session(), &session.ui, include_prompt, session.bash_running.load(Ordering::Relaxed))
            })))
                .into_response()
        }
        Err(_) => (StatusCode::OK, Json(json!({"running": false}))).into_response(),
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
    match session.session().set_session_name(name) {
        Ok(()) => ok(json!({"ok":true})),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    }
}

#[derive(Deserialize, Default)]
pub(super) struct ForkSessionRequest {
    #[serde(default)]
    side: bool,
    name: Option<String>,
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
    let active = { lock(&state.sessions).get(&id).cloned() };
    let mut manager = match active {
        Some(session) => session.session().session_manager().clone(),
        None => match SessionManager::open(&source_info.path) {
            Ok(manager) => manager,
            Err(error) => return refusal(StatusCode::NOT_FOUND, "not_found", format!("Session not found: {error:#}")),
        },
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
        let parent_tools = session_tool_selection(&manager).unwrap_or_else(|| vec!["read".into(), "bash".into()]);
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
    if let Some(active) = lock(&state.sessions).get(&id).cloned() {
        if let Some(path) = active.session().session_file() {
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
            runtime.runtime.dispose().await;
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
    let running_ids = lock(&state.sessions)
        .iter()
        .filter_map(|(id, session)| {
            let agent = session.session();
            (agent.is_streaming() || agent.is_compacting() || session.bash_running.load(Ordering::Relaxed))
                .then(|| id.clone())
        })
        .collect::<HashSet<_>>();
    for id in &input.ids {
        if let Some(active) = lock(&state.sessions).get(id).cloned() {
            if let Some(path) = active.session().session_file() {
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
            runtime.runtime.dispose().await;
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

pub(crate) fn open_session_manager(state: &AppState, id: &str) -> anyhow::Result<SessionManager> {
    if let Some(session) = { lock(&state.sessions).get(id).cloned() } {
        return Ok(session.session().session_manager().clone());
    }
    let info = list_all_sessions(None, &state.agent_dir)
        .into_iter()
        .find(|info| info.id == id)
        .ok_or_else(|| anyhow::anyhow!("Session not found"))?;
    SessionManager::open(&info.path)
}

pub(super) async fn session_entry_thinking(
    State(state): State<AppState>,
    RoutePath((id, entry_id)): RoutePath<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(block_index) = query.get("blockIndex").and_then(|value| value.parse::<usize>().ok()) else {
        return failure(StatusCode::BAD_REQUEST, "Valid blockIndex is required");
    };
    let manager = match open_session_manager(&state, &id) {
        Ok(manager) => manager,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let Some(entry) = manager.entries().iter().find(|entry| entry.id() == entry_id) else {
        return failure(StatusCode::NOT_FOUND, "Assistant message not found");
    };
    let raw = Value::Object(entry.raw().clone());
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
    let manager = match open_session_manager(&state, &id) {
        Ok(manager) => manager,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let Some(entry) = manager.entries().iter().find(|entry| entry.id() == entry_id) else {
        return failure(StatusCode::NOT_FOUND, "Tool result not found");
    };
    let raw = Value::Object(entry.raw().clone());
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
    let snapshot = match pi_coding_agent::export::SessionSnapshot::load(&info.path) {
        Ok(snapshot) => snapshot,
        Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not load session: {error:#}")),
    };
    let data = pi_coding_agent::export::SessionData::from_snapshot(snapshot);
    let html = pi_coding_agent::export::generate_html(&data, &pi_coding_agent::export::ExportTheme::default());
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

fn session_usage_summary(manager: &SessionManager) -> Value {
    use pi_coding_agent::session::EntryData;
    let side_created_at = manager.entries().iter().find_map(|entry| {
        let raw = entry.raw();
        (raw.get("type").and_then(Value::as_str) == Some("custom")
            && raw.get("customType").and_then(Value::as_str) == Some("pi-web:side"))
        .then(|| raw.get("data").and_then(|data| data.get("createdAt")).and_then(Value::as_str))
        .flatten()
    });
    let entries = manager
        .entries()
        .iter()
        .filter(|entry| side_created_at.is_none_or(|created| entry.timestamp() > created))
        .collect::<Vec<_>>();
    let usage = pi_coding_agent::session::usage::usage_totals(entries.iter().copied());
    let tool_calls = entries
        .iter()
        .map(|entry| match entry.data() {
            EntryData::Message(pi_ai::Message::Assistant(message)) => message.tool_calls().count(),
            _ => 0,
        })
        .sum::<usize>();
    let mut active_ms = 0_i64;
    let mut previous = None;
    for entry in &entries {
        if !matches!(entry.kind(), "message" | "compaction" | "branch_summary" | "custom_message") {
            continue;
        }
        let Some(timestamp) = pi_coding_agent::session::parse_iso(entry.timestamp()) else { continue };
        let role = entry.message().map(pi_ai::Message::role).unwrap_or_default();
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
    for entry in manager.get_branch(None) {
        match entry.data() {
            EntryData::ModelChange { provider, model_id } => {
                model = Some(json!({"provider":provider,"modelId":model_id}))
            }
            EntryData::ThinkingLevelChange { thinking_level: level } => thinking_level = level.clone(),
            EntryData::Message(pi_ai::Message::Assistant(message)) => {
                model = Some(json!({"provider":message.provider,"modelId":message.model}));
            }
            _ => {}
        }
    }
    json!({
        "tokens":{"input":usage.input,"output":usage.output,"cacheRead":usage.cache_read,"cacheWrite":usage.cache_write,"total":usage.tokens()},
        "cost":usage.cost,
        "toolCalls":tool_calls,
        "totalActiveMs":active_ms,
        "model":model,
        "thinkingLevel":thinking_level,
    })
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
        if let Ok(manager) = open_session_manager(&state, &id) {
            stats.insert(id, session_usage_summary(&manager));
        }
    }
    ok(json!({"stats":stats}))
}

pub(super) async fn session_context(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let manager = match open_session_manager(&state, &id) {
        Ok(manager) => manager,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let requested_tail = query.get("tail").and_then(|value| value.parse::<i64>().ok()).unwrap_or(50);
    let tail = if requested_tail > 0 { requested_tail.min(1000) as usize } else { 50 };
    let response = session_context::build(
        &manager,
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
    let manager = match open_session_manager(&state, &id) {
        Ok(manager) => manager,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let entries = manager.entries().iter().map(|entry| Value::Object(entry.raw().clone())).collect::<Vec<_>>();
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
    let manager = match open_session_manager(&state, &id) {
        Ok(manager) => manager,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let tree = manager.get_tree();
    let summary_tree = query.get("tree").is_some_and(|value| value == "summary");
    let projected_tree = session_tree_projection::build(&tree, summary_tree);
    let side_created_at = manager.entries().iter().find_map(|entry| {
        let raw = entry.raw();
        (raw.get("type").and_then(Value::as_str) == Some("custom")
            && raw.get("customType").and_then(Value::as_str) == Some("pi-web:side"))
        .then(|| raw.get("data").and_then(|data| data.get("createdAt")).and_then(Value::as_str))
        .flatten()
    });
    let own_entries: Vec<_> = manager
        .entries()
        .iter()
        .filter(|entry| side_created_at.is_none_or(|created| entry.timestamp() > created))
        .collect();
    let usage = pi_coding_agent::session::usage::usage_totals(own_entries.iter().copied());
    let mut stats = (0_usize, 0_usize, 0_usize, 0_usize, 0_usize);
    for entry in &own_entries {
        if let Some(message) = entry.message() {
            stats.4 += 1;
            match message {
                pi_ai::Message::User(_) => stats.0 += 1,
                pi_ai::Message::Assistant(assistant) => {
                    stats.1 += 1;
                    stats.2 += assistant.tool_calls().count();
                }
                pi_ai::Message::ToolResult(_) => stats.3 += 1,
                _ => {}
            }
        }
    }
    let file_path = manager.session_file().map(|path| path.to_string_lossy().to_string()).unwrap_or_default();
    let metadata = manager.session_file().and_then(|path| fs::metadata(path).ok());
    let modified_ms = metadata
        .as_ref()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64);
    let modified = modified_ms.map(pi_coding_agent::session::format_iso).unwrap_or_else(|| manager.header().timestamp);
    let latest_entry_id = manager.entries().last().map(|entry| entry.id());
    use std::hash::{Hash, Hasher};
    let mut revision_hasher = std::collections::hash_map::DefaultHasher::new();
    (metadata.as_ref().map(|meta| meta.len()), modified_ms, manager.entry_count(), latest_entry_id, manager.leaf_id())
        .hash(&mut revision_hasher);
    let mut active_ms = 0_i64;
    let mut previous = None;
    for entry in &own_entries {
        if !matches!(entry.kind(), "message" | "compaction" | "branch_summary" | "custom_message") {
            continue;
        }
        let Some(timestamp) = pi_coding_agent::session::parse_iso(entry.timestamp()) else { continue };
        let role = entry.message().map(pi_ai::Message::role).unwrap_or_default();
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
        &manager,
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
    session_notices::attach(&manager, &mut context, &manager.header().cwd, &state.agent_dir);
    let tool_names = session_tool_selection(&manager);
    let session_header = manager.header();
    let first_message = manager
        .entries()
        .iter()
        .find_map(|entry| {
            if entry.kind() != "message" || entry.raw()["message"]["role"] != "user" {
                return None;
            }
            let content = &entry.raw()["message"]["content"];
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
    let mut info = json!({
        "path":file_path,"id":session_header.id,"cwd":session_header.cwd,
        "created":session_header.timestamp,"modified":modified,"messageCount":stats.4,
        "firstMessage":first_message,"transient":metadata.is_none()
    });
    if let Some(fields) = session_project_fields(&session_header.cwd).as_object() {
        info.as_object_mut().unwrap().extend(fields.clone());
    }
    if let Some(name) = manager.session_name() {
        info["name"] = json!(name);
    }
    let side = manager
        .entries()
        .iter()
        .find(|entry| entry.raw().get("customType").and_then(Value::as_str) == Some("pi-web:side"));
    if let Some(entry) = side {
        let data = &entry.raw()["data"];
        let parent_id = data["parentSessionId"].as_str();
        if let Some(parent_id) = parent_id {
            info["parentSessionId"] = json!(parent_id);
        }
        if let Some(parent_id) = parent_id {
            info["relation"] = json!({"kind":"side","parentSessionId":parent_id});
        }
    } else if let Some(parent_path) = session_header.parent_session.as_ref() {
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
        "leafId":manager.leaf_id(),
        "tree":projected_tree, "info":info,"context":context,
        "stats":{"userMessages":stats.0,"assistantMessages":stats.1,"toolCalls":stats.2,"toolResults":stats.3,"totalMessages":stats.4,
            "tokens":{"input":usage.input,"output":usage.output,"cacheRead":usage.cache_read,"cacheWrite":usage.cache_write,"total":usage.tokens()},"cost":usage.cost},
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
    let busy = sessions.iter().any(|session| {
        let agent = session.session();
        same_session_cwd(agent.cwd(), Path::new(cwd))
            && (!agent.is_idle() || session.bash_running.load(Ordering::Relaxed))
    });
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
    let disposed = {
        let mut sessions = lock(&state.sessions);
        let ids = sessions
            .iter()
            .filter_map(|(id, session)| {
                same_session_cwd(session.session().cwd(), Path::new(&input.cwd)).then(|| id.clone())
            })
            .collect::<Vec<_>>();
        ids.into_iter().filter_map(|id| sessions.remove(&id)).collect::<Vec<_>>()
    };
    let count = disposed.len();
    for session in disposed {
        session.runtime.dispose().await;
    }
    Json(json!({"disposed":count})).into_response()
}

pub(super) async fn auto_name_session(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    let web_session = match get_or_open(&state, &id).await {
        Ok(session) => session,
        Err(_) => return failure(StatusCode::NOT_FOUND, "Session not found"),
    };
    let agent = web_session.session();
    let result = match session_title::generate(agent.clone()).await {
        Ok(result) => result,
        Err(error) => return failure(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    if !lock(&state.sessions).contains_key(&id) {
        return failure(
            StatusCode::CONFLICT,
            "The session was closed while its title was being generated. Please try again.",
        );
    }
    let title = result["title"].as_str().unwrap_or_default();
    if let Err(error) = agent.set_session_name(title) {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"));
    }
    Json(result).into_response()
}

pub(super) async fn events(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    let session = match get_or_open(&state, &id).await {
        Ok(session) => session,
        Err(error) => return failure(StatusCode::NOT_FOUND, format!("{error:#}")),
    };
    let receiver = session.events.subscribe();
    let replay = std::collections::VecDeque::from(session.ui.pending_requests());
    let events = stream::unfold(
        (receiver, tokio::time::interval(std::time::Duration::from_secs(12)), true, replay),
        |(mut receiver, mut heartbeat, connected, mut replay)| async move {
            if connected {
                return Some((
                    Ok::<Event, std::convert::Infallible>(
                        Event::default().data(json!({"type":"connected"}).to_string()),
                    ),
                    (receiver, heartbeat, false, replay),
                ));
            }
            if let Some(value) = replay.pop_front() {
                return Some((Ok(Event::default().data(value.to_string())), (receiver, heartbeat, false, replay)));
            }
            loop {
                tokio::select! {
                    item = receiver.recv() => match item {
                        Ok(value) => return Some((Ok(Event::default().data(value.to_string())), (receiver, heartbeat, false, replay))),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                    _ = heartbeat.tick() => return Some((Ok(Event::default().data(json!({"type":"heartbeat"}).to_string())), (receiver, heartbeat, false, replay))),
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
    match dispatch(&state, &session, command).await {
        Ok(data) => {
            if let Some(new_id) = data.get("sessionId").and_then(Value::as_str) {
                let mut sessions = lock(&state.sessions);
                sessions.remove(&id);
                sessions.insert(new_id.to_string(), session);
            }
            ok(data)
        }
        Err(error) => {
            let mut body = json!({"error": error});
            if is_prompt {
                body["code"] = json!("prompt_rejected");
                body["accepted"] = json!(false);
            }
            (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
        }
    }
}

pub(super) async fn lease(State(state): State<AppState>, RoutePath(id): RoutePath<String>) -> Response {
    let session = lock(&state.sessions).get(&id).cloned();
    match session {
        Some(session) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "running": session.session().is_streaming() || session.session().is_compacting() || session.bash_running.load(Ordering::Relaxed),
                "awaitingInput": session.ui.awaiting_input(),
                "subagentsActive": false,
            })),
        )
            .into_response(),
        None => (
            StatusCode::OK,
            Json(json!({"success": true, "running": false, "awaitingInput": false, "subagentsActive": false})),
        )
            .into_response(),
    }
}

async fn dispatch(state: &AppState, web: &WebSession, value: Value) -> Result<Value, String> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    let session = web.session();
    if kind == "get_tools" {
        let active = session.active_tool_names();
        let declared: HashMap<String, String> = session
            .state()
            .tools
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool.description().to_string()))
            .collect();
        let hidden = session.with_tool_registry(|registry| registry.hidden_declarations().to_vec());
        let tools = session
            .all_tools()
            .into_iter()
            .filter(|tool| tool.exposure != pi_agent::ToolExposure::Hidden)
            .map(|tool| {
                let is_active = active.contains(&tool.name);
                let mut value = json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "promptGuidelines": tool.prompt_guidelines,
                    "exposure": tool.exposure.as_str(),
                    "namespace": tool.namespace,
                    "annotations": tool.annotations,
                    "source": tool.source,
                    "sourcePath": tool.source_path,
                    "active": is_active,
                    "declarationHidden": hidden.contains(&tool.name),
                });
                if let Some(description) = declared.get(&tool.name) {
                    value["description"] = json!(description);
                }
                value
            })
            .collect::<Vec<_>>();
        return Ok(Value::Array(tools));
    }
    if kind == "set_tools" {
        let names = value
            .get("toolNames")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .map(|name| {
                        name.as_str().map(str::to_string).ok_or_else(|| "toolNames must be strings".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        if let Some(names) = &names {
            session.set_active_tools_by_name(names);
        } else {
            let defaults = session.settings().default_tools().unwrap_or_else(|| {
                pi_coding_agent::settings::tools::DEFAULT_TOOL_NAMES.iter().map(|name| name.to_string()).collect()
            });
            session.set_active_tools_by_name(&defaults);
        }
        let data = names
            .as_ref()
            .map(|names| json!({"version":1, "tools":names}))
            .unwrap_or_else(|| json!({"version":1, "cleared":true}));
        session
            .session_manager()
            .append_custom_entry("pi-web:tool-selection", Some(data))
            .map_err(|error| format!("{error:#}"))?;
        return Ok(json!({"sessionId": session.session_id(), "recreated": false}));
    }
    if kind == "abort_compaction" {
        session.abort_compaction();
        return Ok(Value::Null);
    }
    if kind == "navigate_tree" {
        let target = value.get("targetId").and_then(Value::as_str).ok_or("targetId is required")?;
        let result = session
            .navigate_tree(target, NavigateTreeOptions::default())
            .await
            .map_err(|error| format!("{error:#}"))?;
        return Ok(json!({"cancelled": result.cancelled, "aborted": result.aborted, "editorText": result.editor_text}));
    }
    if kind == "get_commands" {
        let mut commands = builtin_commands(&session)
            .into_iter()
            .filter_map(|command| serde_json::to_value(command).ok())
            .collect::<Vec<_>>();
        commands.extend(session.extension_command_infos().as_array().cloned().unwrap_or_default());
        return Ok(json!({"commands": commands}));
    }
    if kind == "extension_ui_response" {
        let id = value.get("id").and_then(Value::as_str).ok_or("id is required")?;
        let mut response = value.as_object().cloned().ok_or("command must be an object")?;
        response.remove("type");
        return Ok(json!({"resolved":web.ui.resolve(id, response)}));
    }
    if kind == "extension_ui_input" {
        let id = value.get("id").and_then(Value::as_str).ok_or("id is required")?;
        let data = value.get("data").and_then(Value::as_str).ok_or("data is required")?;
        return Ok(json!({"accepted":web.ui.input(id, data)}));
    }
    if kind == "reload" {
        if session.is_streaming() || web.bash_running.load(Ordering::Relaxed) {
            return Err("Wait for the current response to finish before reloading.".into());
        }
        let active_tools = session.active_tool_names();
        web.ui.clear_for_reload();
        session.reload_extensions().await.map_err(|error| format!("{error:#}"))?;
        session.set_active_tools_by_name(&active_tools);
        return Ok(json!({"success":true}));
    }
    if kind == "abort" {
        web.ui.cancel_pending();
        let (steering, follow_up) = session.clear_queue();
        session.abort().await;
        return if steering.is_empty() && follow_up.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(json!({"steering":steering, "followUp":follow_up}))
        };
    }
    let requested_clone_leaf = value.get("leafId").and_then(Value::as_str).map(str::to_string);
    let parsed: RpcCommand = serde_json::from_value(value).map_err(|error| error.to_string())?;
    let result = match parsed {
        RpcCommand::Prompt { message, images, streaming_behavior } => {
            if images.as_ref().is_none_or(Vec::is_empty)
                && let Some((name, args)) = split_slash_command(&message)
            {
                if SESSION_COMMANDS.iter().any(|command| command.name == name)
                    && let Some(output) = run_session_command(&session, name, args)
                {
                    let _ = web.events.send(json!({"type":"command_output", "text": output}));
                    return Ok(json!({"disposition":"handled", "agentInvoked":false}));
                }
                if HEADLESS_COMMANDS.iter().any(|command| command.name == name)
                    && let Some(output) = run_headless_command(&session, name, args).await
                {
                    let _ = web.events.send(json!({"type":"command_output", "text": output.text}));
                    return Ok(json!({"disposition":"handled", "agentInvoked":false}));
                }
            }
            let options = PromptOptions {
                images: images.unwrap_or_default(),
                streaming_behavior: streaming_behavior.map(Into::into),
                source: Some("rpc".into()),
                ..PromptOptions::default()
            };
            let disposition = session.prompt_with(&message, options).await.map_err(|error| format!("{error:#}"))?;
            let mut result = json!({"disposition": disposition.as_str()});
            if disposition != PromptDisposition::Queued {
                result["agentInvoked"] = json!(disposition == PromptDisposition::Started);
            }
            Ok(result)
        }
        RpcCommand::Steer { message, images } => {
            let disposition = session
                .queue_user_input(&message, images.unwrap_or_default(), StreamingBehavior::Steer, "rpc")
                .await
                .map_err(|error| format!("{error:#}"))?;
            Ok(json!({"disposition": disposition.as_str()}))
        }
        RpcCommand::FollowUp { message, images } => {
            let disposition = session
                .queue_user_input(&message, images.unwrap_or_default(), StreamingBehavior::FollowUp, "rpc")
                .await
                .map_err(|error| format!("{error:#}"))?;
            Ok(json!({"disposition": disposition.as_str()}))
        }
        RpcCommand::Abort => unreachable!("abort is handled above"),
        RpcCommand::ClearQueue => {
            let (steering, follow_up) = session.clear_queue();
            Ok(json!({"steering": steering, "followUp": follow_up}))
        }
        RpcCommand::GetState => Ok(status_json(&session, &web.ui, true, web.bash_running.load(Ordering::Relaxed))),
        RpcCommand::GetMessages => Ok(json!({"messages": session.messages()})),
        RpcCommand::SetModel { provider, model_id } => {
            let model = session
                .model_runtime()
                .available_models()
                .into_iter()
                .find(|model| model.provider == provider && model.id == model_id)
                .ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?;
            session
                .set_model(model.clone(), session.settings().remember_last_model())
                .await
                .map_err(|error| format!("{error:#}"))?;
            Ok(serde_json::to_value(model).unwrap_or(Value::Null))
        }
        RpcCommand::CycleModel => Ok(session.cycle_model(false, session.settings().remember_last_model()).map_or(
            Value::Null,
            |result| json!({"model": result.model, "thinkingLevel": result.thinking_level, "isScoped": result.is_scoped}),
        )),
        RpcCommand::GetAvailableModels => Ok(json!({"models": session.model_runtime().available_models()})),
        RpcCommand::SetThinkingLevel { level } => {
            session.set_thinking_level(level, false);
            Ok(Value::Null)
        }
        RpcCommand::CycleThinkingLevel => {
            Ok(session.cycle_thinking_level(false).map_or(Value::Null, |level| json!({"level": level})))
        }
        RpcCommand::GetAvailableThinkingLevels => Ok(json!({"levels": session.available_thinking_levels()})),
        RpcCommand::SetFastMode { enabled } => {
            session.set_fast_mode(enabled, false);
            Ok(Value::Null)
        }
        RpcCommand::SetSteeringMode { mode } => {
            session.set_steering_mode(mode);
            Ok(Value::Null)
        }
        RpcCommand::SetFollowUpMode { mode } => {
            session.set_follow_up_mode(mode);
            Ok(Value::Null)
        }
        RpcCommand::Compact { custom_instructions } => {
            let result = session.compact(custom_instructions.as_deref()).await.map_err(|error| format!("{error:#}"))?;
            Ok(pi_coding_agent::agent_session::compaction_result_json(&result))
        }
        RpcCommand::SetAutoCompaction { enabled } => {
            session.set_auto_compaction_enabled(enabled);
            Ok(Value::Null)
        }
        RpcCommand::SetAutoRetry { enabled } => {
            session.set_auto_retry_enabled(enabled);
            Ok(Value::Null)
        }
        RpcCommand::AbortRetry => {
            session.abort_retry();
            Ok(Value::Null)
        }
        RpcCommand::Bash { command, exclude_from_context } => {
            web.bash_running.store(true, Ordering::Relaxed);
            let result = session.execute_bash(&command, exclude_from_context.unwrap_or(false), None, &mut |_| {}).await;
            web.bash_running.store(false, Ordering::Relaxed);
            let result = result.map_err(|error| format!("{error:#}"))?;
            Ok(
                json!({"output": result.output, "exitCode": result.exit_code, "cancelled": result.cancelled, "truncated": result.truncated, "fullOutputPath": result.full_output_path.map(|path| path.display().to_string())}),
            )
        }
        RpcCommand::AbortBash => {
            session.abort_bash();
            Ok(Value::Null)
        }
        RpcCommand::GetSessionStats => Ok(session.session_stats().to_json()),
        RpcCommand::GetLastAssistantText => Ok(json!({"text": session.last_assistant_text()})),
        RpcCommand::SetSessionName { name } => {
            session.set_session_name(&name).map_err(|error| format!("{error:#}"))?;
            Ok(Value::Null)
        }
        RpcCommand::GetEntries { since } => {
            let manager = session.session_manager();
            let entries = manager.entries();
            let start = match since {
                Some(since) => entries
                    .iter()
                    .position(|entry| entry.id() == since)
                    .map(|index| index + 1)
                    .ok_or_else(|| format!("Entry not found: {since}"))?,
                None => 0,
            };
            Ok(
                json!({"entries": entries[start..].iter().map(|entry| Value::Object(entry.raw().clone())).collect::<Vec<_>>(), "leafId": manager.leaf_id()}),
            )
        }
        RpcCommand::GetForkMessages => Ok(
            json!({"messages": session.user_messages_for_forking().into_iter().map(|(entry_id, text)| json!({"entryId": entry_id, "text": text})).collect::<Vec<_>>()}),
        ),
        RpcCommand::GetTree => {
            serde_json::from_str(&pi_coding_agent::rpc::tree_data_json(&session.session_manager()))
                .map_err(|error| error.to_string())
        }
        RpcCommand::ExportHtml { output_path } => session
            .export_to_html(output_path.as_deref())
            .map(|path| json!({"path": path.display().to_string()}))
            .map_err(|error| format!("{error:#}")),
        RpcCommand::NewSession { parent_session } => web
            .runtime
            .new_session(parent_session, None)
            .await
            .map(|result| json!({"cancelled": result.cancelled}))
            .map_err(|error| format!("{error:#}")),
        RpcCommand::SwitchSession { session_path } => web
            .runtime
            .switch_session(std::path::Path::new(&session_path), None)
            .await
            .map(|result| json!({"cancelled": result.cancelled}))
            .map_err(|error| format!("{error:#}")),
        RpcCommand::Fork { entry_id } => create_fork_session(state, web, &entry_id).await,
        RpcCommand::CloneSession => {
            create_clone_session(state, web, requested_clone_leaf.as_deref()).await
        }
        RpcCommand::GetCommands => unreachable!("get_commands is handled above"),
    };
    if matches!(kind.as_str(), "new_session" | "switch_session")
        && result.as_ref().is_ok_and(|value| value.get("cancelled").and_then(Value::as_bool) != Some(true))
    {
        let new_id = web.session().session_id();
        // The map update is performed by the request handler, which owns AppState.
        return result.map(|mut value| {
            value["sessionId"] = json!(new_id);
            value
        });
    }
    result
}

async fn create_fork_session(state: &AppState, web: &WebSession, entry_id: &str) -> Result<Value, String> {
    let source = web.session();
    let (current_file, session_dir, parent) = {
        let manager = source.session_manager();
        if !manager.is_persisted() {
            return Ok(json!({"cancelled":true, "cancelReason":"unsaved"}));
        }
        let current_file = manager.session_file().ok_or("Persisted session is missing a session file")?.to_path_buf();
        let session_dir = manager.session_dir().to_path_buf();
        let entry = manager.get_entry(entry_id).ok_or("Invalid entry ID for forking")?;
        if !matches!(entry.data(), pi_coding_agent::session::entry::EntryData::Message(pi_ai::Message::User(_))) {
            return Err("Invalid entry ID for forking".into());
        }
        let parent = entry.parent_id().map(str::to_string);
        (current_file, session_dir, parent)
    };
    let selected_text =
        source.user_messages_for_forking().into_iter().find(|(id, _)| id == entry_id).map(|(_, text)| text);
    if let Some(runner) = source.extension_runner().filter(|runner| runner.has_handlers("session_before_fork")) {
        let result = runner.emit(json!({"type":"session_before_fork", "entryId":entry_id, "position":"before"})).await;
        if result["cancel"].as_bool() == Some(true) {
            return Ok(json!({"cancelled":true, "cancelReason":"extension"}));
        }
    }
    let new_manager = match parent.as_deref() {
        None => SessionManager::create_with(
            source.cwd(),
            Some(&session_dir),
            NewSessionOptions { id: None, parent_session: Some(current_file.display().to_string()) },
        )
        .map_err(|error| format!("{error:#}"))?,
        Some(parent) => {
            if !current_file.exists() {
                return Err("This session has not been saved yet. Send a message before forking it.".into());
            }
            let mut branched = SessionManager::open_with(&current_file, Some(&session_dir), None)
                .map_err(|error| format!("{error:#}"))?;
            let path = branched
                .create_branched_session(parent)
                .map_err(|error| format!("{error:#}"))?
                .ok_or("Failed to create forked session")?;
            SessionManager::open_with(&path, Some(&session_dir), None).map_err(|error| format!("{error:#}"))?
        }
    };
    let forked = insert_runtime(state, new_manager).await.map_err(|error| format!("{error:#}"))?;
    let mut result = json!({"cancelled":false, "newSessionId":forked.session().session_id()});
    if let Some(text) = selected_text {
        result["text"] = json!(text);
    }
    Ok(result)
}

async fn create_clone_session(
    state: &AppState,
    web: &WebSession,
    requested_leaf: Option<&str>,
) -> Result<Value, String> {
    let source = web.session();
    let (leaf, current_file, session_dir) = {
        let manager = source.session_manager();
        let leaf = requested_leaf.or_else(|| manager.leaf_id()).ok_or("Cannot clone an empty session")?.to_string();
        if !manager.get_branch(Some(&leaf)).iter().any(|entry| {
            matches!(entry.data(), pi_coding_agent::session::entry::EntryData::Message(pi_ai::Message::Assistant(_)))
        }) {
            return Ok(json!({"cancelled":true}));
        }
        if !manager.is_persisted() {
            return Ok(json!({"cancelled":true}));
        }
        let current_file = manager.session_file().ok_or("Persisted session is missing a session file")?.to_path_buf();
        if !current_file.exists() {
            return Ok(json!({"cancelled":true}));
        }
        (leaf, current_file, manager.session_dir().to_path_buf())
    };
    if let Some(runner) = source.extension_runner().filter(|runner| runner.has_handlers("session_before_fork")) {
        let result = runner.emit(json!({"type":"session_before_fork", "entryId":leaf, "position":"at"})).await;
        if result["cancel"].as_bool() == Some(true) {
            return Ok(json!({"cancelled":true, "byExtension":true}));
        }
    }
    let mut source_manager =
        SessionManager::open_with(&current_file, Some(&session_dir), None).map_err(|error| format!("{error:#}"))?;
    let path = source_manager
        .create_branched_session(&leaf)
        .map_err(|error| format!("{error:#}"))?
        .ok_or("Failed to create cloned session")?;
    let manager = SessionManager::open_with(&path, Some(&session_dir), None).map_err(|error| format!("{error:#}"))?;
    let clone = insert_runtime(state, manager).await.map_err(|error| format!("{error:#}"))?;
    Ok(json!({"cancelled":false, "newSessionId":clone.session().session_id()}))
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
    let sessions = lock(&state.sessions);
    let running = sessions
        .iter()
        .filter_map(|(id, session)| {
            let agent = session.session();
            (agent.is_streaming() || agent.is_compacting() || session.bash_running.load(Ordering::Relaxed))
                .then(|| id.clone())
        })
        .collect::<Vec<_>>();
    let awaiting =
        sessions.iter().filter_map(|(id, session)| session.ui.awaiting_input().then(|| id.clone())).collect::<Vec<_>>();
    (StatusCode::OK, Json(json!({"sessionListVersion":session_list_version,"runningSessionIds": running, "awaitingInputSessionIds": awaiting,"sessionUiStateRevision":session_ui_state::revision(&state.agent_dir)}))).into_response()
}

pub(super) async fn debug_snapshot(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let now = pi_ai::now_ms();
    let requested = query.get("session").map(String::as_str);
    let sessions = lock(&state.sessions);
    let mut summaries = Vec::with_capacity(sessions.len());
    let mut detail = None;
    for (id, web) in sessions.iter() {
        let agent = web.session();
        let streaming = agent.is_streaming();
        let compacting = agent.is_compacting();
        let bash_running = web.bash_running.load(Ordering::Relaxed);
        let awaiting_input = web.ui.awaiting_input();
        let queued = agent.steering_messages().len() + agent.follow_up_messages().len();
        let snapshot = json!({
            "sessionId":id,
            "state":"alive",
            "stale":false,
            "subagent":false,
            "chatOnly":false,
            "createdAt":web.created_at,
            "startedAt":web.created_at,
            "closingAt":null,
            "extensions":"bound",
            "run":{
                "running":streaming || compacting || bash_running,
                "streaming":streaming,
                "promptRunning":streaming,
                "pendingPrompts":0,
                "bashRunning":bash_running,
                "compacting":compacting,
                "retrying":false,
                "queuedMessages":queued,
                "waitingForMcp":false,
                "runningTools":0,
                "mutatingCommands":0,
                "replacement":null
            },
            "idle":{"timeoutMs":0,"nextCheckAt":null,"lastResetAt":null,"forcedByStop":false},
            "awaitingInput":awaiting_input,
            "pendingUiRequests":lock(&web.ui.pending).len(),
            "customUis":lock(&web.ui.custom).len(),
            "eventListeners":web.events.receiver_count(),
            "lastEvent":null,
            "lastCommand":null
        });
        summaries.push(json!({
            "sessionId":id,
            "state":"alive",
            "running":streaming || compacting || bash_running,
            "awaitingInput":awaiting_input,
            "subagent":false,
            "eventStreams":web.events.receiver_count(),
            "providers":0,
            "nextIdleCheckAt":null,
            "forcedByStop":false,
            "starts":1
        }));
        if requested == Some(id.as_str()) {
            detail = Some(json!({
                "sessionId":id,
                "wrapper":snapshot,
                "lifecycle":{"starts":1,"firstStartedAt":web.created_at,"lastStartedAt":web.created_at,"lastClosedAt":null},
                "starting":false,
                "providers":null,
                "eventStreams":[],
                "subagents":{"active":false,"runs":[],"notifications":[]},
                "mcp":null
            }));
        }
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
