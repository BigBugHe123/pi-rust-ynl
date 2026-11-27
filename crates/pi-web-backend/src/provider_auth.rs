use super::*;

use crate::app_state::{AppState, PendingLoginCallback, failure, lock};
use pi_coding_agent::agent_session::{ModelRuntime, ModelRuntimeOptions};
use pi_coding_agent::settings::{SettingsManager, SettingsOptions};

pub(super) async fn auth_providers(State(state): State<AppState>) -> Response {
    // Building a Pi runtime loads settings, credentials, extensions and provider
    // metadata. The configuration page requests this on every mount, so reuse
    // the immutable provider listing briefly instead of repeating that startup.
    if let Some((created_at, value)) = lock(&state.auth_providers_cache).as_ref()
        && created_at.elapsed() < Duration::from_secs(30)
    {
        return Json(value.clone()).into_response();
    }
    let model_runtime = ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&state.agent_dir)).await;
    let mut entries: HashMap<String, Value> = HashMap::new();
    for (id, name) in model_runtime.all_providers() {
        entries.insert(id.clone(), json!({"id":id,"name":name,"hasApiKeyLogin":false,"hasOAuth":false,"modelCount":model_runtime.all_models().iter().filter(|model|model.provider==id).count()}));
    }
    let credentials = model_runtime.list_credentials().await.unwrap_or_default();
    let credential_types = credentials
        .into_iter()
        .map(|credential| (credential.provider_id, credential.auth_type))
        .collect::<HashMap<_, _>>();
    for login in model_runtime.login_providers(None).await {
        let entry = entries.entry(login.id.clone()).or_insert_with(
            || json!({"id":login.id,"name":login.name,"hasApiKeyLogin":false,"hasOAuth":false,"modelCount":0}),
        );
        match login.auth_type {
            pi_ai::auth::AuthType::OAuth => {
                entry["hasOAuth"] = json!(true);
                if let Some(label) = login.login_label {
                    entry["oauthName"] = json!(label);
                }
            }
            pi_ai::auth::AuthType::ApiKey => entry["hasApiKeyLogin"] = json!(!login.ambient),
        }
        if let Some(status) = login.status {
            entry["status"] = json!({"configured":true,"source":status.source});
        }
        if let Some(credential_type) = credential_types.get(&login.id) {
            entry["credentialType"] = json!(match credential_type {
                pi_ai::auth::AuthType::OAuth => "oauth",
                pi_ai::auth::AuthType::ApiKey => "api_key",
            });
        }
    }

    let mut oauth_providers = Vec::new();
    let mut api_key_providers = Vec::new();
    let mut provider_names = serde_json::Map::new();
    for (id, entry) in entries {
        let name = entry.get("name").and_then(Value::as_str).unwrap_or(&id);
        provider_names.insert(id.clone(), json!(name));
        let has_api_key = entry.get("hasApiKeyLogin").and_then(Value::as_bool).unwrap_or(false);
        let has_oauth = entry.get("hasOAuth").and_then(Value::as_bool).unwrap_or(false);
        let credential_type = entry.get("credentialType").and_then(Value::as_str);
        let status = entry.get("status");
        let configured = status.and_then(|status| status.get("configured")).and_then(Value::as_bool).unwrap_or(false);
        let source = status.and_then(|status| status.get("source")).and_then(Value::as_str);
        if has_oauth {
            let display_name = match id.as_str() {
                "openai-codex" => "ChatGPT Plus/Pro (legacy)",
                "github-copilot" => "GitHub Copilot",
                _ => entry.get("oauthName").and_then(Value::as_str).unwrap_or(name),
            };
            oauth_providers.push(json!({"id":id,"name":display_name,"usesCallbackServer":false,"loggedIn":credential_type==Some("oauth"),"supportsApiKey":has_api_key}));
        }
        if has_api_key && !matches!(source, Some("models_json_key" | "models_json_command" | "configured API key")) {
            let key_configured = configured && credential_type != Some("oauth");
            let mut provider = json!({"id":id,"displayName":name,"configured":key_configured,"modelCount":entry.get("modelCount").and_then(Value::as_u64).unwrap_or(0),"supportsOAuth":has_oauth});
            if key_configured {
                if let Some(source) = source {
                    provider["source"] = json!(source);
                }
            }
            api_key_providers.push(provider);
        }
    }
    oauth_providers
        .sort_by_key(|provider| provider.get("name").and_then(Value::as_str).unwrap_or_default().to_lowercase());
    api_key_providers
        .sort_by_key(|provider| provider.get("displayName").and_then(Value::as_str).unwrap_or_default().to_lowercase());
    let value = json!({"providers":oauth_providers,"oauthProviders":oauth_providers,"apiKeyProviders":api_key_providers,"providerNames":provider_names});
    *lock(&state.auth_providers_cache) = Some((std::time::Instant::now(), value.clone()));
    Json(value).into_response()
}

pub(super) struct ApiKeyInteraction {
    api_key: String,
    router_url: Option<String>,
    key_submitted: AtomicBool,
}

#[async_trait]
impl AuthInteraction for ApiKeyInteraction {
    async fn prompt(&self, prompt: AuthPrompt, cancel: CancellationToken) -> AuthResult<String> {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        match prompt {
            AuthPrompt::Text { .. } if self.router_url.is_some() => Ok(self.router_url.clone().unwrap_or_default()),
            AuthPrompt::Select { options, .. } => options
                .iter()
                .find(|option| matches!(option.id.as_str(), "api-key" | "bearer-token"))
                .map(|option| option.id.clone())
                .ok_or_else(|| AuthError::other("Provider requires interactive authentication setup")),
            AuthPrompt::Secret { .. } if !self.key_submitted.swap(true, Ordering::SeqCst) => {
                Ok(self.api_key.trim().to_owned())
            }
            _ => Err(AuthError::other("Provider requires additional authentication settings")),
        }
    }

    fn notify(&self, _event: AuthEvent) {}
}

pub(super) async fn api_key_auth(
    State(state): State<AppState>,
    RoutePath(provider): RoutePath<String>,
    request: Request,
) -> Response {
    let bytes = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let input: Value = match serde_json::from_slice(&bytes) {
        Ok(input) => input,
        Err(_) => return failure(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };
    let api_key = input.get("apiKey").and_then(Value::as_str).unwrap_or_default();
    let llama = provider == pi_coding_agent::llama::LLAMA_PROVIDER_ID;
    if input.get("apiKey").is_some_and(|value| !value.is_string()) || (!llama && api_key.trim().is_empty()) {
        return failure(StatusCode::BAD_REQUEST, "apiKey is required");
    }
    let router_url = if llama {
        let Some(value) = input.get("serverUrl").and_then(Value::as_str) else {
            return failure(StatusCode::BAD_REQUEST, "Enter an http or https llama.cpp router URL");
        };
        match reqwest::Url::parse(value) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => Some(url.to_string()),
            _ => return failure(StatusCode::BAD_REQUEST, "Enter an http or https llama.cpp router URL"),
        }
    } else {
        None
    };

    let model_runtime = ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&state.agent_dir)).await;
    let interaction =
        ApiKeyInteraction { api_key: api_key.to_owned(), router_url, key_submitted: AtomicBool::new(false) };
    if let Err(error) = model_runtime
        .login(&provider, AuthType::ApiKey, &interaction, &LoginOptions::default(), CancellationToken::new())
        .await
    {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    }

    let catalog = if llama {
        if std::env::var_os("PI_OFFLINE").is_some() {
            json!({"completed":false,"changed":false,"reason":"offline"})
        } else {
            let before = model_runtime
                .all_models()
                .into_iter()
                .map(|model| format!("{}\0{}\0{}", model.provider, model.id, model.name))
                .collect::<Vec<_>>();
            let refresh = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                model_runtime.refresh_catalogs(true, Some(vec![provider.clone()]), true),
            )
            .await;
            let after = model_runtime
                .all_models()
                .into_iter()
                .map(|model| format!("{}\0{}\0{}", model.provider, model.id, model.name))
                .collect::<Vec<_>>();
            let mut before = before;
            let mut after = after;
            before.sort();
            after.sort();
            let changed = before != after;
            match refresh {
                Ok(result) => json!({"completed":!result.aborted && result.errors.is_empty(),"changed":changed}),
                Err(_) => json!({"completed":false,"changed":changed,"reason":"runtime"}),
            }
        }
    } else {
        Value::Null
    };
    if llama {
        Json(json!({"success":true,"catalog":catalog})).into_response()
    } else {
        Json(json!({"success":true})).into_response()
    }
}

pub(super) async fn delete_provider_credential(
    State(state): State<AppState>,
    RoutePath((provider, expected)): RoutePath<(String, String)>,
) -> Response {
    let model_runtime = ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&state.agent_dir)).await;
    let expected_type = match expected.as_str() {
        "api-key" => AuthType::ApiKey,
        "oauth" => AuthType::OAuth,
        _ => {
            return failure(StatusCode::NOT_FOUND, "Unknown credential type");
        }
    };
    if expected_type == AuthType::OAuth && model_runtime.resolver().oauth(&provider).is_none() {
        return failure(StatusCode::BAD_REQUEST, format!("Unknown provider: {provider}"));
    }
    let store = model_runtime.credential_store();
    let stored = match store.read(&provider).await {
        Ok(stored) => stored,
        Err(error) => {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
    };
    if let Some(credential) = stored {
        if credential.auth_type() != expected_type {
            let message = match expected_type {
                AuthType::OAuth => format!("{provider} is authenticated with an API key, not OAuth"),
                AuthType::ApiKey => format!("{provider} is authenticated with OAuth, not an API key"),
            };
            return failure(StatusCode::CONFLICT, message);
        }
        if let Err(error) = model_runtime.logout(&provider).await {
            return failure(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
    }
    if expected_type == AuthType::OAuth {
        Json(json!({"ok":true})).into_response()
    } else {
        Json(json!({"success":true})).into_response()
    }
}

pub(super) struct PendingLoginInput {
    token: String,
    receiver: oneshot::Receiver<String>,
}

#[derive(Default)]
pub(super) struct ManualLoginInput {
    token: Option<String>,
    receiver: Option<oneshot::Receiver<String>>,
}

pub(super) struct OAuthInteraction {
    provider: String,
    callbacks: Arc<Mutex<HashMap<String, PendingLoginCallback>>>,
    events: mpsc::UnboundedSender<Value>,
    manual: Mutex<ManualLoginInput>,
    tokens: Mutex<HashSet<String>>,
}

impl OAuthInteraction {
    fn create_input(&self) -> Result<PendingLoginInput, String> {
        let token = random_login_token(&self.provider)?;
        let (sender, receiver) = oneshot::channel();
        lock(&self.callbacks).insert(token.clone(), PendingLoginCallback { provider: self.provider.clone(), sender });
        lock(&self.tokens).insert(token.clone());
        Ok(PendingLoginInput { token, receiver })
    }

    fn manual_input(&self) -> Result<PendingLoginInput, String> {
        let mut manual = lock(&self.manual);
        if let (Some(token), Some(receiver)) = (manual.token.clone(), manual.receiver.take()) {
            return Ok(PendingLoginInput { token, receiver });
        }
        let request = self.create_input()?;
        manual.token = Some(request.token.clone());
        Ok(request)
    }

    fn ensure_manual_token(&self) -> Result<String, String> {
        let mut manual = lock(&self.manual);
        if let Some(token) = &manual.token {
            return Ok(token.clone());
        }
        let request = self.create_input()?;
        let token = request.token.clone();
        manual.token = Some(token.clone());
        manual.receiver = Some(request.receiver);
        Ok(token)
    }

    fn cancel_input(&self, token: &str) {
        if let Some(callback) = lock(&self.callbacks).remove(token) {
            let _ = callback.sender.send(String::new());
        }
        lock(&self.tokens).remove(token);
        let mut manual = lock(&self.manual);
        if manual.token.as_deref() == Some(token) {
            *manual = ManualLoginInput::default();
        }
    }

    fn cleanup(&self) {
        let tokens = std::mem::take(&mut *lock(&self.tokens));
        let mut callbacks = lock(&self.callbacks);
        for token in tokens {
            if let Some(callback) = callbacks.remove(&token) {
                let _ = callback.sender.send(String::new());
            }
        }
        drop(callbacks);
        *lock(&self.manual) = ManualLoginInput::default();
    }

    async fn wait_for_input(&self, request: PendingLoginInput, cancel: CancellationToken) -> AuthResult<String> {
        let token = request.token;
        let result = tokio::select! {
            value = request.receiver => value.map_err(|_| AuthError::Cancelled),
            () = cancel.cancelled() => Err(AuthError::Cancelled),
        };
        self.cancel_input(&token);
        result
    }
}

#[async_trait]
impl AuthInteraction for OAuthInteraction {
    async fn prompt(&self, prompt: AuthPrompt, cancel: CancellationToken) -> AuthResult<String> {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let manual = matches!(prompt, AuthPrompt::ManualCode { .. });
        let request = if manual { self.manual_input() } else { self.create_input() }.map_err(AuthError::other)?;
        match prompt {
            AuthPrompt::Select { message, options } => {
                let _ = self
                    .events
                    .send(json!({"type":"select_request","message":message,"options":options,"token":request.token}));
            }
            AuthPrompt::Text { message, placeholder }
            | AuthPrompt::Secret { message, placeholder }
            | AuthPrompt::ManualCode { message, placeholder } => {
                let _ = self.events.send(
                    json!({"type":"prompt_request","message":message,"placeholder":placeholder,"token":request.token}),
                );
            }
        }
        self.wait_for_input(request, cancel).await
    }

    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                if let Ok(token) = self.ensure_manual_token() {
                    let _ =
                        self.events.send(json!({"type":"auth","url":url,"instructions":instructions,"token":token}));
                }
            }
            AuthEvent::DeviceCode { user_code, verification_uri, interval_seconds, expires_in_seconds, .. } => {
                let _ = self.events.send(json!({"type":"device_code","userCode":user_code,"verificationUri":verification_uri,"intervalSeconds":interval_seconds,"expiresInSeconds":expires_in_seconds}));
            }
            AuthEvent::Info { message, .. } | AuthEvent::Progress { message } => {
                let _ = self.events.send(json!({"type":"progress","message":message}));
            }
        }
    }
}

fn random_login_token(provider: &str) -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    SystemRandom::new().fill(&mut bytes).map_err(|_| "Unable to generate a login token".to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let uuid = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    );
    Ok(format!("{provider}-{uuid}"))
}

pub(super) async fn oauth_login_callback(
    State(state): State<AppState>,
    RoutePath(provider): RoutePath<String>,
    Json(input): Json<Value>,
) -> Response {
    let Some(token) = input.get("token").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "token and code required");
    };
    let Some(code) = input.get("code").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        return failure(StatusCode::BAD_REQUEST, "token and code required");
    };
    let mut callbacks = lock(&state.login_callbacks);
    let Some(pending) = callbacks.get(token) else {
        return failure(StatusCode::NOT_FOUND, "No pending login for token");
    };
    if pending.provider != provider || !token.starts_with(&format!("{provider}-")) {
        return failure(StatusCode::BAD_REQUEST, "Token does not match provider");
    }
    let pending = callbacks.remove(token).expect("pending login callback was checked");
    if pending.sender.send(code.to_owned()).is_err() {
        return failure(StatusCode::NOT_FOUND, "No pending login for token");
    }
    Json(json!({"ok":true,"provider":provider})).into_response()
}

pub(super) async fn oauth_login(State(state): State<AppState>, RoutePath(provider): RoutePath<String>) -> Response {
    let (events, receiver) = mpsc::unbounded_channel::<Value>();
    let callbacks = state.login_callbacks.clone();
    let stop = state.stop.clone();
    tokio::spawn(async move {
        let model_runtime = tokio::select! {
            result = ModelRuntime::create(ModelRuntimeOptions::for_agent_dir(&state.agent_dir)) => result,
            () = events.closed() => return,
            () = stop.cancelled() => return,
        };
        if model_runtime.resolver().oauth(&provider).is_none() {
            let _ = events.send(json!({"type":"error","message":format!("Unknown provider: {provider}")}));
            return;
        }
        let settings = SettingsManager::create(&state.agent_dir, &state.agent_dir, SettingsOptions::default());
        let device_id = settings.get_or_create_device_id();
        settings.flush();
        let options = LoginOptions { device_id: Some(Arc::new(move || device_id.clone())), ..LoginOptions::default() };
        let interaction = Arc::new(OAuthInteraction {
            provider: provider.clone(),
            callbacks,
            events: events.clone(),
            manual: Mutex::new(ManualLoginInput::default()),
            tokens: Mutex::new(HashSet::new()),
        });
        let cancel = CancellationToken::new();
        let mut login =
            Box::pin(model_runtime.login(&provider, AuthType::OAuth, interaction.as_ref(), &options, cancel.clone()));
        let disconnected = tokio::select! {
            result = &mut login => match result {
                Ok(_) => { let _ = events.send(json!({"type":"success"})); false }
                Err(error) if error.to_string() == "Login cancelled" => { let _ = events.send(json!({"type":"cancelled"})); false }
                Err(error) => { let _ = events.send(json!({"type":"error","message":error.to_string()})); false }
            },
            () = events.closed() => true,
            () = stop.cancelled() => true,
        };
        if disconnected {
            cancel.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut login).await;
        }
        drop(login);
        interaction.cleanup();
    });
    let body = stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|value| (Ok::<Event, std::convert::Infallible>(Event::default().data(value.to_string())), receiver))
    });
    let mut response = Sse::new(body).keep_alive(KeepAlive::default()).into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}
