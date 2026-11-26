use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use axum::Json;
use axum::extract::Path as RoutePath;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pi_coding_agent::agent_session::{ModelRuntime, ModelRuntimeOptions};
use pi_coding_agent::config::agent_dir;
use pi_mcp::config::{
    McpConfigScope, McpServerEntry, get_mcp_tool_exposure, load_mcp_config, project_mcp_config,
    validate_mcp_server_config,
};
use pi_mcp::config_value::resolve_config_value_or_err;
use pi_mcp::error::OAuthError;
use pi_mcp::oauth::{LoopbackCallbackFactory, McpOAuthState, McpOAuthStateStore};
use pi_mcp::runtime::{
    ConnectionOptions, McpOAuthCredentialStore, McpOAuthServerStore, McpServerConnection, ServerState,
};
use pi_mcp::runtime::{McpSignInPrompt, SignInOptions, sign_in_mcp_server};
use pi_mcp::types::{Implementation, Tool};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::web_backend::{AppState, failure};

struct ConnectableEntry {
    scope: McpConfigScope,
    name: String,
    cwd: PathBuf,
    source_path: String,
    config_key: String,
    config: Value,
}

fn connectable_refusal(status: StatusCode, reason: &str, error: impl Into<String>) -> Response {
    (status, Json(json!({"error":error.into(),"reason":reason}))).into_response()
}

fn operator_disables_mcp() -> bool {
    std::env::var("PI_WEB_DISABLE_MCP").ok().is_some_and(|value| {
        !value.trim().is_empty() && !matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "false")
    })
}

fn resolve_connectable_entry(state: &AppState, input: &Value) -> Result<ConnectableEntry, Response> {
    let scope = match input.get("scope").and_then(Value::as_str) {
        Some("global") => McpConfigScope::Global,
        Some("project") => McpConfigScope::Project,
        _ => {
            return Err(connectable_refusal(
                StatusCode::BAD_REQUEST,
                "invalid-request",
                "scope must be \"global\" or \"project\"",
            ));
        }
    };
    let Some(name) = input
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.len() <= 1024)
        .map(str::to_owned)
    else {
        return Err(connectable_refusal(StatusCode::BAD_REQUEST, "invalid-request", "name must be a server name"));
    };

    let requested_cwd = input.get("cwd").and_then(Value::as_str);
    if scope == McpConfigScope::Project && requested_cwd.is_none() {
        return Err(connectable_refusal(
            StatusCode::BAD_REQUEST,
            "invalid-request",
            "A project server needs the project's cwd",
        ));
    }
    let cwd = if let Some(requested_cwd) = requested_cwd {
        let candidate = PathBuf::from(requested_cwd);
        if !candidate.is_absolute() {
            return Err(connectable_refusal(StatusCode::BAD_REQUEST, "cwd-invalid", "cwd must be an absolute path"));
        }
        let Some(canonical) = crate::web_backend::path_access::allowed_existing_path(state, &candidate) else {
            return Err(connectable_refusal(StatusCode::FORBIDDEN, "cwd-denied", "Access denied"));
        };
        if !canonical.is_dir() {
            return Err(connectable_refusal(StatusCode::BAD_REQUEST, "cwd-not-directory", "cwd must be a directory"));
        }
        canonical
    } else {
        pi_coding_agent::config::home_dir()
    };

    if operator_disables_mcp() {
        return Err(connectable_refusal(
            StatusCode::CONFLICT,
            "mcp-off",
            "PI_WEB_DISABLE_MCP is set, so Pi Web connects no MCP server",
        ));
    }
    let project_trusted = scope == McpConfigScope::Project && project_is_trusted(&cwd);
    if scope == McpConfigScope::Project && !project_trusted {
        return Err(connectable_refusal(
            StatusCode::FORBIDDEN,
            "project-untrusted",
            "The project is not trusted, so Pi Web does not start its MCP servers",
        ));
    }
    if scope == McpConfigScope::Project {
        let project_file = project_mcp_config(&cwd);
        if project_file.exists()
            && crate::web_backend::path_access::allowed_existing_path(state, &project_file).is_none()
        {
            return Err(connectable_refusal(
                StatusCode::FORBIDDEN,
                "cwd-denied",
                "Project MCP config resolves outside the folders Pi Web may read",
            ));
        }
    }

    let loaded = load_mcp_config(&state.agent_dir, &cwd, project_trusted);
    let Some(entry) = loaded.servers.iter().find(|entry| entry.name == name) else {
        if let Some(problem) = loaded.errors.iter().find(|error| error.contains(&format!("\"{name}\""))) {
            return Err(connectable_refusal(StatusCode::CONFLICT, "server-invalid", problem.clone()));
        }
        if let Some(problem) = loaded.errors.first() {
            return Err(connectable_refusal(StatusCode::INTERNAL_SERVER_ERROR, "internal", problem.clone()));
        }
        return Err(connectable_refusal(
            StatusCode::CONFLICT,
            "server-missing",
            format!("MCP server \"{name}\" is not listed"),
        ));
    };
    if scope == McpConfigScope::Project && entry.scope != Some(McpConfigScope::Project) && entry.override_path.is_none()
    {
        return Err(connectable_refusal(
            StatusCode::CONFLICT,
            "server-missing",
            format!("MCP server \"{name}\" is not listed in the project configuration"),
        ));
    }
    let source_path = entry.override_path.as_deref().unwrap_or(&entry.source).to_owned();
    let config = serde_json::to_value(&entry.config).unwrap_or(Value::Null);
    let (_, _, web_password_field) = super::config_read::config_value_references(&config);
    if web_password_field.is_some() {
        return Err(connectable_refusal(
            StatusCode::CONFLICT,
            "web-password",
            format!("\"{name}\" references PI_WEB_PASSWORD, so Pi Web does not connect it"),
        ));
    }
    let config_key = super::ui_state::config_key(&config);
    Ok(ConnectableEntry { scope, name, cwd, source_path, config_key, config })
}

fn connection_input(input: &Value, entry: &ConnectableEntry) -> Value {
    let mut resolved = input.clone();
    resolved["scope"] = json!(entry.scope.to_string());
    resolved["name"] = json!(entry.name);
    resolved["cwd"] = json!(entry.cwd);
    resolved["sourcePath"] = json!(entry.source_path);
    resolved["configKey"] = json!(entry.config_key);
    resolved["config"] = entry.config.clone();
    resolved
}

const DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
const REQUEST_TIMEOUT_SECONDS: f64 = 15.0;
const MAX_TOOLS: usize = 500;
const MASK: &str = "•••";

struct SignInFlow {
    name: String,
    scope: String,
    url: String,
    started: Instant,
    ended_at: Mutex<Option<Instant>>,
    value: Mutex<Value>,
    redirect: Mutex<Option<String>>,
    expected_state: Mutex<Option<String>>,
    redirect_url: Mutex<Option<Url>>,
    cancel: CancellationToken,
    notify: Notify,
}

fn sign_in_flows() -> &'static Mutex<HashMap<String, Arc<SignInFlow>>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, Arc<SignInFlow>>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn flow_info(flow: &SignInFlow) -> Value {
    let mut value = flow.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let phase = value.get("phase").and_then(Value::as_str).unwrap_or("connecting");
    value["expiresInMs"] = json!(if matches!(phase, "done" | "failed" | "cancelled" | "expired") {
        0
    } else {
        300_000u64.saturating_sub(flow.started.elapsed().as_millis() as u64)
    });
    value
}

fn set_flow(flow: &SignInFlow, phase: &str, extra: Value) {
    let mut value = flow.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    value["phase"] = json!(phase);
    if matches!(phase, "done" | "failed" | "cancelled" | "expired") {
        *flow.ended_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    }
    if let (Some(target), Some(fields)) = (value.as_object_mut(), extra.as_object()) {
        for (key, field) in fields {
            target.insert(key.clone(), field.clone());
        }
    }
    flow.notify.notify_waiters();
}

struct BackendSignInPrompt(Arc<SignInFlow>);

struct GuardedSignInStore {
    inner: Arc<McpOAuthServerStore>,
    cancel: CancellationToken,
}

#[async_trait::async_trait]
impl McpOAuthStateStore for GuardedSignInStore {
    async fn load(&self) -> Result<Option<McpOAuthState>, OAuthError> {
        self.inner.load().await
    }

    async fn save(&self, state: McpOAuthState) -> Result<(), OAuthError> {
        let inner = self.inner.clone();
        let cancel = self.cancel.clone();
        self.inner
            .with_refresh_lock(async move {
                if cancel.is_cancelled() {
                    return Err(OAuthError::Cancelled);
                }
                inner.save(state).await
            })
            .await?
    }
}

#[async_trait::async_trait]
impl McpSignInPrompt for BackendSignInPrompt {
    fn show_authorization_url(&self, _server: &str, url: &Url) {
        let redirect = url.query_pairs().find(|(key, _)| key == "redirect_uri").map(|(_, value)| value.into_owned());
        let expected = url.query_pairs().find(|(key, _)| key == "state").map(|(_, value)| value.into_owned());
        *self.0.redirect_url.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            redirect.as_deref().and_then(|value| Url::parse(value).ok());
        *self.0.expected_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = expected;
        set_flow(&self.0, "authorize", json!({"authorizationUrl":url.as_str(),"redirectUrl":redirect}));
    }

    async fn prompt_for_redirect_url(&self, _server: &str, cancel: CancellationToken) -> Option<String> {
        loop {
            let notified = self.0.notify.notified();
            if let Some(value) = self.0.redirect.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
                return Some(value);
            }
            tokio::select! { _ = notified => {}, _ = cancel.cancelled() => return None, _ = self.0.cancel.cancelled() => return None }
        }
    }
}

async fn run_sign_in_flow(flow: Arc<SignInFlow>, input: Value) {
    set_flow(&flow, "connecting", Value::Null);
    let scope = if flow.scope == "project" { McpConfigScope::Project } else { McpConfigScope::Global };
    let cwd =
        input.get("cwd").and_then(Value::as_str).map(PathBuf::from).unwrap_or_else(pi_coding_agent::config::home_dir);
    if flow.scope == "project" && !project_is_trusted(&cwd) {
        set_flow(&flow, "failed", json!({"failure":"project-untrusted","error":"Project resources are not trusted"}));
        return;
    }
    let (mut first, connection) = connect(input.clone(), flow.name.clone(), cwd.clone(), scope, DEADLINE).await;
    first["testedAt"] = json!(pi_ai::now_ms());
    let Some(connection) = connection else {
        set_flow(&flow, "failed", json!({"failure":"connect-failed","result":first}));
        return;
    };
    if first.get("state").and_then(Value::as_str) == Some("connected") {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection.close()).await;
        set_flow(&flow, "done", json!({"alreadySignedIn":true,"result":first}));
        return;
    }
    if first.get("state").and_then(Value::as_str) != Some("needs-auth") {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection.close()).await;
        set_flow(&flow, "failed", json!({"failure":"connect-failed","result":first}));
        return;
    }
    let settings = match connection.oauth_settings() {
        Ok(settings) => settings,
        Err(error) => {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection.close()).await;
            set_flow(
                &flow,
                "failed",
                json!({"failure":"sign-in-failed","error":clip(redact_config_secrets(&error, input.get("config").unwrap_or(&Value::Null), &[]), 2000)}),
            );
            return;
        }
    };
    let resolved = settings.client_secret.clone().into_iter().collect::<Vec<_>>();
    let credentials = McpOAuthCredentialStore::new(&agent_dir());
    let store = Arc::new(GuardedSignInStore {
        inner: credentials.for_server(&flow.name, &flow.url),
        cancel: flow.cancel.clone(),
    });
    let prompt = Arc::new(BackendSignInPrompt(flow.clone()));
    set_flow(&flow, "starting", Value::Null);
    let result = sign_in_mcp_server(SignInOptions {
        server_name: flow.name.clone(),
        server_url: Url::parse(&flow.url).expect("validated MCP OAuth URL"),
        store,
        settings,
        challenge: connection.challenge(),
        prompt,
        cancel: flow.cancel.clone(),
        callback_factory: Arc::new(LoopbackCallbackFactory),
        client: pi_ai::http_client().clone(),
    })
    .await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection.close()).await;
    if let Err(error) = result {
        let text = redact_config_secrets(&error.to_string(), input.get("config").unwrap_or(&Value::Null), &resolved);
        let phase = if flow.cancel.is_cancelled() { "cancelled" } else { "failed" };
        let details =
            if phase == "failed" { json!({"failure":"sign-in-failed","error":clip(text, 2000)}) } else { Value::Null };
        set_flow(&flow, phase, details);
        return;
    }
    if flow.cancel.is_cancelled() {
        set_flow(&flow, "cancelled", Value::Null);
        return;
    }
    if flow.scope == "project" && !project_is_trusted(&cwd) {
        set_flow(
            &flow,
            "done",
            json!({"result":{"state":"blocked","error":"Project resources are not trusted","testedAt":pi_ai::now_ms()}}),
        );
        return;
    }
    set_flow(&flow, "finishing", Value::Null);
    let (mut after, connection) = connect(input.clone(), flow.name.clone(), cwd, scope, DEADLINE).await;
    if let Some(connection) = connection {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection.close()).await;
    }
    if after.get("state").and_then(Value::as_str) == Some("connected") {
        after["afterSignIn"] = json!(true);
        after["testedAt"] = json!(pi_ai::now_ms());
        if flow.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get("authorizationUrl").is_none() {
            after["refreshed"] = json!(true);
        }
        set_flow(&flow, "done", json!({"result":after}));
    } else {
        after["testedAt"] = json!(pi_ai::now_ms());
        set_flow(&flow, "done", json!({"result":after}));
    }
}

fn project_is_trusted(cwd: &std::path::Path) -> bool {
    crate::web_backend::project_trust::is_trusted_for_resources(cwd, &agent_dir())
}

fn prune_sign_in_flows(flows: &mut HashMap<String, Arc<SignInFlow>>) {
    flows.retain(|_, flow| {
        flow.ended_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none_or(|ended| ended.elapsed() < std::time::Duration::from_secs(60))
    });
    if flows.len() >= 100 {
        let mut ended = flows
            .iter()
            .filter_map(|(id, flow)| {
                flow.ended_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).map(|at| (id.clone(), at))
            })
            .collect::<Vec<_>>();
        ended.sort_by_key(|(_, at)| *at);
        for (id, _) in ended.into_iter().take(flows.len().saturating_sub(99)) {
            flows.remove(&id);
        }
    }
}

pub async fn start_mcp_sign_in(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let connectable = match resolve_connectable_entry(&state, &input) {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let input = connection_input(&input, &connectable);
    let scope = connectable.scope.to_string();
    let name = connectable.name.clone();
    let config = match input.get("config").and_then(Value::as_object) {
        Some(config) => config,
        None => return failure(StatusCode::INTERNAL_SERVER_ERROR, "resolved MCP config is unavailable"),
    };
    let Some(url) = config.get("url").and_then(Value::as_str).filter(|url| !url.is_empty()).map(str::to_string) else {
        return failure(StatusCode::CONFLICT, format!("MCP server \"{name}\" does not use OAuth"));
    };
    if config.get("auth").is_some()
        || config
            .get("headers")
            .and_then(Value::as_object)
            .is_some_and(|headers| headers.keys().any(|key| key.eq_ignore_ascii_case("authorization")))
    {
        return failure(StatusCode::CONFLICT, format!("MCP server \"{name}\" does not use OAuth"));
    }
    let Ok(parsed) = Url::parse(&url) else { return failure(StatusCode::BAD_REQUEST, "invalid OAuth URL") };
    if !matches!(parsed.scheme(), "https" | "http") {
        return failure(StatusCode::BAD_REQUEST, "OAuth URL must use HTTP or HTTPS");
    }
    let key = format!("{name}\0{url}");
    {
        let flows = sign_in_flows().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(flow) = flows.values().find(|flow| {
            format!("{}\0{}", flow.name, flow.url) == key
                && !matches!(
                    flow.value
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get("phase")
                        .and_then(Value::as_str),
                    Some("done" | "failed" | "cancelled" | "expired")
                )
        }) {
            let mut info = flow_info(flow);
            info["joined"] = json!(true);
            return Json(info).into_response();
        }
    }
    let id = pi_coding_agent::session::ids::uuid_v4();
    let source_path = json!(connectable.source_path);
    let config_key = json!(connectable.config_key);
    let flow = Arc::new(SignInFlow {
        name: name.clone(),
        scope: scope.to_string(),
        url,
        started: Instant::now(),
        ended_at: Mutex::new(None),
        value: Mutex::new(
            json!({"flowId":id,"scope":scope,"name":name,"sourcePath":source_path,"configKey":config_key,"phase":"connecting","expiresInMs":300000}),
        ),
        redirect: Mutex::new(None),
        expected_state: Mutex::new(None),
        redirect_url: Mutex::new(None),
        cancel: CancellationToken::new(),
        notify: Notify::new(),
    });
    {
        let mut flows = sign_in_flows().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_sign_in_flows(&mut flows);
        flows.insert(id, flow.clone());
    }
    let run_flow = flow.clone();
    tokio::spawn(async move { run_sign_in_flow(run_flow, input).await });
    let expiry_flow = flow.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        let phase = expiry_flow
            .value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if matches!(phase.as_str(), "done" | "failed" | "cancelled" | "expired") {
            return;
        }
        if phase == "finishing" {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let current = expiry_flow
                .value
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if matches!(current.as_str(), "done" | "failed" | "cancelled" | "expired") {
                return;
            }
        }
        set_flow(&expiry_flow, "expired", Value::Null);
        expiry_flow.cancel.cancel();
    });
    Json(flow_info(&flow)).into_response()
}

pub async fn get_mcp_sign_in(RoutePath(id): RoutePath<String>) -> Response {
    let flow = sign_in_flows().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&id).cloned();
    match flow {
        Some(flow) => {
            let info = flow_info(&flow);
            let phase = info.get("phase").and_then(Value::as_str).unwrap_or_default();
            if matches!(phase, "done" | "failed" | "cancelled" | "expired") {
                let value = flow.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
                if let (Some(scope), Some(source_path), Some(name), Some(config_key), Some(result)) = (
                    value.get("scope").and_then(Value::as_str),
                    value.get("sourcePath").and_then(Value::as_str),
                    value.get("name").and_then(Value::as_str),
                    value.get("configKey").and_then(Value::as_str),
                    value.get("result"),
                ) {
                    if result.is_object() {
                        let mut status = result.clone();
                        status["origin"] = json!("test");
                        let _ = super::ui_state::record_status(Json(json!({
                            "scope":scope,"sourcePath":source_path,"name":name,"configKey":config_key,"status":status
                        })))
                        .await;
                    }
                }
            }
            Json(info).into_response()
        }
        None => failure(StatusCode::NOT_FOUND, "sign-in unknown"),
    }
}

pub async fn redirect_mcp_sign_in(RoutePath(id): RoutePath<String>, Json(input): Json<Value>) -> Response {
    let flow = sign_in_flows().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&id).cloned();
    let Some(flow) = flow else { return failure(StatusCode::NOT_FOUND, "sign-in unknown") };
    let Some(text) = input
        .get("redirectUrl")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty() && text.len() <= 16 * 1024)
    else {
        return failure(StatusCode::BAD_REQUEST, "redirectUrl required");
    };
    let Ok(url) = Url::parse(text.trim()) else {
        return failure(StatusCode::BAD_REQUEST, "Expected the full redirected address from the browser address bar");
    };
    let expected_url = flow.redirect_url.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let expected_state = flow.expected_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let Some(expected_url) = expected_url else {
        return failure(StatusCode::CONFLICT, "Sign-in is not waiting for redirect");
    };
    if url.origin() != expected_url.origin() || url.path() != expected_url.path() {
        return failure(StatusCode::BAD_REQUEST, "The address does not match this sign-in's redirect URI");
    }
    let param = |key: &str| url.query_pairs().find(|(name, _)| name == key).map(|(_, value)| value.into_owned());
    if param("state") != expected_state {
        return failure(StatusCode::BAD_REQUEST, "The address belongs to a different sign-in");
    }
    if let Some(error) = param("error") {
        return failure(StatusCode::BAD_REQUEST, param("error_description").unwrap_or(error));
    }
    if param("code").is_none_or(|code| code.is_empty()) {
        return failure(StatusCode::BAD_REQUEST, "The address does not contain an authorization code");
    }
    *flow.redirect.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text.trim().to_string());
    set_flow(&flow, "finishing", Value::Null);
    Json(flow_info(&flow)).into_response()
}

pub async fn cancel_mcp_sign_in(RoutePath(id): RoutePath<String>) -> Response {
    let flow = sign_in_flows().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&id).cloned();
    let Some(flow) = flow else { return failure(StatusCode::NOT_FOUND, "sign-in unknown") };
    set_flow(&flow, "cancelled", Value::Null);
    flow.cancel.cancel();
    Json(flow_info(&flow)).into_response()
}

fn is_record(value: &Value) -> bool {
    value.is_object()
}

fn field_mentions_password(value: &str) -> bool {
    if value.starts_with('!') {
        return value.to_ascii_uppercase().contains("PI_WEB_PASSWORD");
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        index += 1;
        let braced = bytes.get(index) == Some(&b'{');
        if braced {
            index += 1;
        }
        let start = index;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
            index += 1;
        }
        let name = &value[start..index];
        if name.eq_ignore_ascii_case("PI_WEB_PASSWORD") {
            return true;
        }
        if braced && bytes.get(index) == Some(&b'}') {
            index += 1;
        }
    }
    false
}

fn references_web_password(config: &Value) -> bool {
    let Some(object) = config.as_object() else { return false };
    let mut values = Vec::new();
    if object.contains_key("url") {
        if let Some(headers) = object.get("headers").and_then(Value::as_object) {
            values.extend(headers.values().filter_map(Value::as_str));
        }
        if let Some(secret) = object
            .get("oauth")
            .and_then(Value::as_object)
            .and_then(|oauth| oauth.get("clientSecret"))
            .and_then(Value::as_str)
        {
            values.push(secret);
        }
    } else if let Some(env) = object.get("env").and_then(Value::as_object) {
        values.extend(env.values().filter_map(Value::as_str));
    }
    values.into_iter().any(field_mentions_password)
}

fn is_reference_only(value: &str) -> bool {
    let value =
        value.trim().strip_prefix("Bearer ").or_else(|| value.trim().strip_prefix("bearer ")).unwrap_or(value.trim());
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut found = false;
    while index < bytes.len() {
        if bytes[index] == b'$' {
            index += 1;
            let braced = bytes.get(index) == Some(&b'{');
            if braced {
                index += 1;
            }
            let start = index;
            while bytes.get(index).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
                index += 1;
            }
            if index > start {
                if braced && bytes.get(index) == Some(&b'}') {
                    index += 1;
                }
                found = true;
                continue;
            }
            index = index.saturating_sub(1);
        }
        if !bytes[index].is_ascii_whitespace() && !matches!(bytes[index], b':' | b',' | b';') {
            return false;
        }
        index += 1;
    }
    found
}

fn secret_name(name: &str) -> bool {
    let normalized: String =
        name.chars().flat_map(char::to_lowercase).filter(|ch| ch.is_ascii_alphanumeric()).collect();
    [
        "key",
        "token",
        "secret",
        "password",
        "passwd",
        "passphrase",
        "credential",
        "authorization",
        "bearer",
        "jwt",
        "cookie",
        "signature",
        "pat",
    ]
    .iter()
    .any(|word| normalized.contains(word))
}

fn mask_value(value: &str) -> bool {
    if value.len() < 24 || value.contains(char::is_whitespace) || value.starts_with(['/', '.', '~']) {
        return false;
    }
    let mut classes = [false; 4];
    for ch in value.chars() {
        classes[0] |= ch.is_ascii_lowercase();
        classes[1] |= ch.is_ascii_uppercase();
        classes[2] |= ch.is_ascii_digit();
        classes[3] |= !ch.is_ascii_alphanumeric();
    }
    classes[..3].iter().filter(|found| **found).count() >= 3
}

fn redact_config_secrets(text: &str, raw: &Value, resolved: &[String]) -> String {
    let mut secrets = Vec::<String>::new();
    secrets.extend(resolved.iter().cloned());
    if let Some(object) = raw.as_object() {
        if let Some(env) = object.get("env").and_then(Value::as_object) {
            for (name, value) in env {
                if let Some(value) = value.as_str() {
                    if !is_reference_only(value) && !value.starts_with('!') {
                        secrets.push(value.trim().to_string());
                    }
                    if secret_name(name) && value.len() >= 6 {
                        secrets.push(value.to_string());
                    }
                }
            }
        }
        if let Some(headers) = object.get("headers").and_then(Value::as_object) {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    if !is_reference_only(value) && !value.starts_with('!') {
                        secrets.push(value.trim().to_string());
                    }
                    if secret_name(name) && value.len() >= 6 {
                        secrets.push(value.to_string());
                    }
                }
            }
        }
        if let Some(secret) = object
            .get("oauth")
            .and_then(Value::as_object)
            .and_then(|oauth| oauth.get("clientSecret"))
            .and_then(Value::as_str)
        {
            if !is_reference_only(secret) && !secret.starts_with('!') {
                secrets.push(secret.trim().to_string());
            }
        }
        for key in ["command", "url"] {
            if let Some(value) = object.get(key).and_then(Value::as_str) {
                if key == "command"
                    && (value.contains("--token") || value.contains("--api-key") || value.contains("--password"))
                {
                    secrets.extend(value.split_whitespace().skip(1).filter(|part| part.len() >= 6).map(str::to_string));
                }
                if let Some((_, query)) = value.split_once('?') {
                    for part in query.split(['&', '#']) {
                        if let Some((name, value)) = part.split_once('=') {
                            if secret_name(name) || mask_value(value) {
                                secrets.push(value.into());
                            }
                        }
                    }
                }
            }
        }
        if let Some(args) = object.get("args").and_then(Value::as_array) {
            let mut mask_next = false;
            for arg in args.iter().filter_map(Value::as_str) {
                if mask_next {
                    if arg.len() >= 6 {
                        secrets.push(arg.into());
                    }
                    mask_next = false;
                    continue;
                }
                let (name, value) = arg.split_once('=').unwrap_or((arg, ""));
                if secret_name(name) {
                    if value.is_empty() {
                        mask_next = true;
                    } else if value.len() >= 6 {
                        secrets.push(value.into());
                    }
                } else if mask_value(arg) {
                    secrets.push(arg.into());
                }
            }
        }
    }
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    let mut output = text.to_string();
    for secret in secrets.into_iter().filter(|secret| secret.len() >= 6) {
        output = output.replace(&secret, MASK);
    }
    output
}

fn resolve_values(config: &mut Value, name: &str) -> Result<Vec<String>, String> {
    let mut resolved = Vec::new();
    let is_http = config.get("url").is_some();
    let fields = if is_http { ["headers", "oauth"] } else { ["env", ""] };
    for field in fields.into_iter().filter(|field| !field.is_empty()) {
        let Some(values) = config.get_mut(field).and_then(Value::as_object_mut) else { continue };
        let value_keys: Vec<String> =
            if field == "oauth" { vec!["clientSecret".into()] } else { values.keys().cloned().collect() };
        for key in value_keys {
            let Some(value) = values.get(&key).and_then(Value::as_str).map(str::to_string) else { continue };
            let description =
                if field == "oauth" { "oauth.clientSecret".to_string() } else { format!("{field} \"{key}\"") };
            let resolved_value =
                resolve_config_value_or_err(&value, &format!("MCP server \"{name}\" {description}"), None)?;
            resolved.push(resolved_value.clone());
            values.insert(key, json!(resolved_value));
        }
    }
    Ok(resolved)
}

fn clip(value: String, max: usize) -> String {
    if value.chars().count() <= max {
        return value;
    }
    let clipped: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{clipped}…")
}

fn describe_tool(tool: &Tool, config: &pi_mcp::McpServerConfig, raw: &Value, resolved: &[String]) -> Value {
    let name = redact_config_secrets(&tool.name, raw, resolved);
    let description = tool
        .description
        .as_deref()
        .or(tool.title.as_deref())
        .or_else(|| tool.annotations.as_ref().and_then(|value| value.get("title")).and_then(Value::as_str))
        .map(|description| description.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("").to_string())
        .filter(|description| !description.is_empty());
    let read_only =
        tool.annotations.as_ref().and_then(|value| value.get("readOnlyHint")).and_then(Value::as_bool).unwrap_or(false);
    let exposure = get_mcp_tool_exposure(config, &tool.name).to_string();
    json!({"name": clip(name, 300), "description": description.map(|value| clip(value, 300)), "readOnly":read_only,"exposure":exposure})
}

fn server_info(info: &Implementation, raw: &Value, resolved: &[String]) -> Value {
    let redact = |value: &str| clip(redact_config_secrets(value, raw, resolved), 300);
    let mut result = json!({"name":redact(&info.name),"version":redact(&info.version)});
    if let Some(title) = info.title.as_deref().filter(|title| !title.is_empty()) {
        result["title"] = json!(redact(title));
    }
    result
}

async fn connect(
    raw: Value,
    name: String,
    cwd: PathBuf,
    scope: McpConfigScope,
    timeout: std::time::Duration,
) -> (Value, Option<McpServerConnection>) {
    let started = Instant::now();
    let Some(raw_config) = raw.get("config") else {
        return (
            json!({"state":"failed","error":"MCP config is required","tools":[],"toolCount":0,"durationMs":0}),
            None,
        );
    };
    if !is_record(raw_config) {
        return (
            json!({"state":"failed","error":"MCP config must be an object","tools":[],"toolCount":0,"durationMs":0}),
            None,
        );
    }
    if references_web_password(raw_config) {
        return (
            json!({"state":"failed","error":"MCP config references PI_WEB_PASSWORD, which the backend will not pass to MCP servers","tools":[],"toolCount":0,"durationMs":0}),
            None,
        );
    }
    let mut resolved_config = raw_config.clone();
    let resolved_secrets = match resolve_values(&mut resolved_config, &name) {
        Ok(values) => values,
        Err(error) => {
            let message = redact_config_secrets(&error, raw_config, &[]);
            return (
                json!({"state":"failed","error":clip(message,2000),"tools":[],"toolCount":0,"durationMs":started.elapsed().as_millis() as u64}),
                None,
            );
        }
    };
    let mut config = match validate_mcp_server_config(&name, &resolved_config) {
        Ok(config) => config,
        Err(error) => return (json!({"state":"failed","error":error,"tools":[],"toolCount":0,"durationMs":0}), None),
    };
    config.timeout = Some(config.timeout.unwrap_or(60.0).min(REQUEST_TIMEOUT_SECONDS));
    let source = raw.get("sourcePath").and_then(Value::as_str).unwrap_or_default();
    let mut entry = McpServerEntry::new(name, config.clone(), source);
    entry.scope = Some(scope);
    let credentials = McpOAuthCredentialStore::new(&agent_dir());
    let mut options = ConnectionOptions::new(entry, cwd.clone(), credentials);
    let models = Arc::new(ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&agent_dir())).await);
    options.provider_token = Some(Arc::new(move |provider| {
        let models = models.clone();
        Box::pin(async move { models.api_key_for(&provider).await })
    }));
    let connection = McpServerConnection::new(options);
    let result = tokio::time::timeout(timeout, connection.get_client()).await;
    let duration = started.elapsed().as_millis() as u64;
    let is_stdio = config.url.is_none();
    let state = match result {
        Ok(Ok(client)) => {
            let tools = connection.tools();
            let listed: Vec<Value> = tools
                .iter()
                .take(MAX_TOOLS)
                .map(|tool| {
                    let mut value = describe_tool(tool, &config, raw_config, &resolved_secrets);
                    if let Some(description) = value.get("description").and_then(Value::as_str) {
                        value["description"] =
                            json!(clip(redact_config_secrets(description, raw_config, &resolved_secrets), 300));
                    }
                    value
                })
                .collect();
            let mut body = json!({"state":"connected","tools":listed,"toolCount":tools.len(),"durationMs":duration,"resources":connection.has_resources()});
            if !connection.has_resources() {
                body.as_object_mut().unwrap().remove("resources");
            }
            if let Some(info) = client.server_info() {
                body["serverInfo"] = server_info(&info, raw_config, &resolved_secrets);
            }
            if is_stdio {
                body["cwd"] = json!(cwd);
            }
            body
        }
        Ok(Err(error)) => {
            let state = if connection.state() == ServerState::NeedsAuth { "needs-auth" } else { "failed" };
            let message = connection.error().unwrap_or_else(|| error.to_string());
            let mut body = json!({"state":state,"error":clip(redact_config_secrets(&message, raw_config, &resolved_secrets),2000),"tools":[],"toolCount":0,"durationMs":duration});
            if is_stdio {
                body["cwd"] = json!(cwd);
            }
            body
        }
        Err(_) => {
            let mut body = json!({"state":"failed","timedOut":true,"tools":[],"toolCount":0,"durationMs":duration});
            if is_stdio {
                body["cwd"] = json!(cwd);
            }
            body
        }
    };
    (state, Some(connection))
}

pub async fn test_mcp_server(State(state): State<AppState>, Json(input): Json<Value>) -> Response {
    let connectable = match resolve_connectable_entry(&state, &input) {
        Ok(entry) => entry,
        Err(response) => return response,
    };
    let request = connection_input(&input, &connectable);
    let (mut result, connection) =
        connect(request, connectable.name.clone(), connectable.cwd.clone(), connectable.scope, DEADLINE).await;
    if let Some(connection) = connection {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection.close()).await;
    }
    result["testedAt"] = json!(pi_ai::now_ms());
    let status = {
        let mut status = result.clone();
        status["origin"] = json!("test");
        status
    };
    let _ = super::ui_state::record_status(Json(json!({
        "scope":connectable.scope.to_string(),
        "sourcePath":connectable.source_path,
        "name":connectable.name,
        "configKey":connectable.config_key,
        "status":status
    })))
    .await;
    Json(json!({"scope":connectable.scope.to_string(),"name":connectable.name,"configKey":connectable.config_key,"result":result}))
        .into_response()
}

pub async fn validate_mcp_server(Json(input): Json<Value>) -> Response {
    if let Some(servers) = input.get("servers").and_then(Value::as_array) {
        let results = servers
            .iter()
            .map(|item| {
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                let config = item.get("config").unwrap_or(&Value::Null);
                match validate_mcp_server_config(name, config) {
                    Ok(config) => json!({"name":name,"valid":true,"config":config}),
                    Err(error) => json!({"name":name,"valid":false,"error":error}),
                }
            })
            .collect::<Vec<_>>();
        return Json(json!({"results":results})).into_response();
    }
    let Some(name) = input.get("name").and_then(Value::as_str) else {
        return failure(StatusCode::BAD_REQUEST, "name required");
    };
    let Some(config) = input.get("config") else {
        return failure(StatusCode::BAD_REQUEST, "MCP config required");
    };
    match validate_mcp_server_config(name, config) {
        Ok(config) => Json(json!({"valid":true,"config":config})).into_response(),
        Err(error) => Json(json!({"valid":false,"error":error})).into_response(),
    }
}

pub async fn sign_out_mcp_server(Json(input): Json<Value>) -> Response {
    let Some(name) = input.get("name").and_then(Value::as_str).filter(|value| !value.is_empty() && value.len() <= 1024)
    else {
        return failure(StatusCode::BAD_REQUEST, "name required");
    };
    let Some(url) = input.get("url").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "url required");
    };
    let Ok(parsed) = url::Url::parse(url) else { return failure(StatusCode::BAD_REQUEST, "invalid url") };
    if !matches!(parsed.scheme(), "http" | "https") {
        return failure(StatusCode::BAD_REQUEST, "MCP OAuth URL must use HTTP or HTTPS");
    }
    let active = sign_in_flows()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter(|flow| flow.name == name && flow.url == url)
        .cloned()
        .collect::<Vec<_>>();
    for flow in active {
        set_flow(&flow, "cancelled", Value::Null);
        flow.cancel.cancel();
    }
    let directory = agent_dir();
    if !directory.join("mcp-auth.json").is_file() {
        return Json(json!({"removed":false})).into_response();
    }
    let credentials = McpOAuthCredentialStore::new(&directory);
    let server = credentials.for_server(name, url);
    match server.with_refresh_lock(async { credentials.remove(name, url) }).await {
        Ok(removed) => Json(json!({"removed": removed})).into_response(),
        Err(error) => failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}
