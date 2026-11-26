use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_coding_agent::extension_ui::{DialogOptions, ExtensionUi, NotifyType, WidgetPlacement};
use pi_coding_agent::extensions::{CustomRequest, JsComponent};
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, oneshot};

use crate::web_backend::app_state::lock;

pub(crate) struct WebExtensionUi {
    pub(crate) events: broadcast::Sender<Value>,
    pub(crate) pending: Mutex<HashMap<String, oneshot::Sender<Map<String, Value>>>>,
    pending_requests: Mutex<HashMap<String, Value>>,
    pub(crate) custom: Mutex<HashMap<String, Arc<Mutex<JsComponent>>>>,
    pub(crate) statuses: Mutex<HashMap<String, String>>,
    pub(crate) widgets: Mutex<HashMap<String, Value>>,
}

impl WebExtensionUi {
    pub(crate) fn new(events: broadcast::Sender<Value>) -> Arc<Self> {
        Arc::new(Self {
            events,
            pending: Mutex::new(HashMap::new()),
            pending_requests: Mutex::new(HashMap::new()),
            custom: Mutex::new(HashMap::new()),
            statuses: Mutex::new(HashMap::new()),
            widgets: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn emit(&self, value: Value) {
        let _ = self.events.send(value);
    }

    async fn dialog(&self, method: &str, mut fields: Value, options: DialogOptions) -> Option<Map<String, Value>> {
        let cancel = options.cancel.unwrap_or_default();
        if cancel.is_cancelled() {
            return None;
        }
        let id = pi_coding_agent::session::ids::uuid_v4();
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id.clone(), sender);
        fields["type"] = json!("extension_ui_request");
        fields["id"] = json!(id);
        fields["method"] = json!(method);
        lock(&self.pending_requests).insert(id.clone(), fields.clone());
        self.emit(fields);
        let timeout = async {
            match options.timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending().await,
            }
        };
        let result = tokio::select! {
            response = receiver => response.ok(),
            _ = cancel.cancelled() => None,
            _ = timeout => None,
        };
        lock(&self.pending).remove(&id);
        lock(&self.pending_requests).remove(&id);
        result.filter(|response| response.get("cancelled").and_then(Value::as_bool) != Some(true))
    }

    pub(crate) fn resolve(&self, id: &str, mut response: Map<String, Value>) -> bool {
        lock(&self.pending_requests).remove(id);
        if let Some(sender) = lock(&self.pending).remove(id) {
            // Keep the ID in the response for parity with the JSON-RPC UI protocol.
            response.insert("id".into(), json!(id));
            return sender.send(response).is_ok();
        }
        false
    }

    pub(crate) fn pending_requests(&self) -> Vec<Value> {
        lock(&self.pending_requests).values().cloned().collect()
    }

    pub(crate) fn awaiting_input(&self) -> bool {
        !lock(&self.pending).is_empty() || !lock(&self.custom).is_empty()
    }

    pub(crate) fn input(&self, id: &str, data: &str) -> bool {
        let custom = lock(&self.custom).get(id).cloned();
        let Some(custom) = custom else { return false };
        let mut component = lock(&custom);
        pi_tui::Component::handle_input(&mut *component, data);
        let lines = pi_tui::Component::render(&mut *component, 80);
        drop(component);
        self.emit(json!({"type":"extension_ui_request", "id":id, "method":"custom", "lines":lines}));
        true
    }

    pub(crate) fn clear_for_reload(&self) {
        self.cancel_pending();
        let statuses = std::mem::take(&mut *lock(&self.statuses));
        let widgets = std::mem::take(&mut *lock(&self.widgets));
        for key in statuses.keys() {
            self.emit(json!({"type":"extension_ui_request", "id":pi_coding_agent::session::ids::uuid_v4(), "method":"setStatus", "statusKey":key, "statusText":null}));
        }
        for key in widgets.keys() {
            self.emit(json!({"type":"extension_ui_request", "id":pi_coding_agent::session::ids::uuid_v4(), "method":"setWidget", "widgetKey":key, "widgetLines":[]}));
        }
    }

    pub(crate) fn cancel_pending(&self) {
        for (id, sender) in std::mem::take(&mut *lock(&self.pending)) {
            lock(&self.pending_requests).remove(&id);
            let mut response = Map::new();
            response.insert("cancelled".into(), json!(true));
            response.insert("id".into(), json!(id));
            let _ = sender.send(response);
        }
    }
}

#[async_trait]
impl ExtensionUi for WebExtensionUi {
    async fn select(&self, title: &str, options: &[String], opts: DialogOptions) -> Option<String> {
        self.dialog(
            "select",
            json!({"title":title, "options":options, "timeout":opts.timeout.map(|d| d.as_millis() as u64)}),
            opts,
        )
        .await?
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_string)
    }

    async fn confirm(&self, title: &str, message: &str, opts: DialogOptions) -> bool {
        self.dialog(
            "confirm",
            json!({"title":title, "message":message, "timeout":opts.timeout.map(|d| d.as_millis() as u64)}),
            opts,
        )
        .await
        .and_then(|response| response.get("confirmed").and_then(Value::as_bool))
        .unwrap_or(false)
    }

    async fn input(&self, title: &str, placeholder: Option<&str>, opts: DialogOptions) -> Option<String> {
        let mut fields =
            json!({"title":title, "placeholder":placeholder, "timeout":opts.timeout.map(|d| d.as_millis() as u64)});
        if let Some(prefill) = &opts.prefill {
            fields["prefill"] = json!(prefill);
        }
        if opts.secret {
            fields["secret"] = json!(true);
        }
        self.dialog("input", fields, opts).await?.get("value").and_then(Value::as_str).map(str::to_string)
    }

    async fn editor(&self, title: &str, prefill: Option<&str>, opts: DialogOptions) -> Option<String> {
        self.dialog(
            "editor",
            json!({"title":title, "prefill":prefill, "timeout":opts.timeout.map(|d| d.as_millis() as u64)}),
            opts,
        )
        .await?
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_string)
    }

    fn notify(&self, message: &str, kind: Option<NotifyType>) {
        self.emit(json!({"type":"extension_ui_request", "id":pi_coding_agent::session::ids::uuid_v4(), "method":"notify", "message":message, "notifyType":kind.map(NotifyType::as_str)}));
    }

    fn set_status(&self, key: &str, text: Option<&str>) {
        let mut statuses = lock(&self.statuses);
        if let Some(text) = text {
            statuses.insert(key.to_string(), text.to_string());
        } else {
            statuses.remove(key);
        }
        self.emit(json!({"type":"extension_ui_request", "id":pi_coding_agent::session::ids::uuid_v4(), "method":"setStatus", "statusKey":key, "statusText":text}));
    }

    fn set_widget(&self, key: &str, lines: Option<&[String]>, placement: Option<WidgetPlacement>) {
        let mut widgets = lock(&self.widgets);
        if let Some(lines) = lines {
            widgets.insert(key.to_string(), json!({"key":key, "lines":lines, "placement":placement.map(WidgetPlacement::as_str).unwrap_or("aboveEditor")}));
        } else {
            widgets.remove(key);
        }
        self.emit(json!({"type":"extension_ui_request", "id":pi_coding_agent::session::ids::uuid_v4(), "method":"setWidget", "widgetKey":key, "widgetLines":lines, "widgetPlacement":placement.map(WidgetPlacement::as_str)}));
    }

    fn set_title(&self, title: &str) {
        self.emit(json!({"type":"extension_ui_request", "id":pi_coding_agent::session::ids::uuid_v4(), "method":"setTitle", "title":title}));
    }

    fn set_editor_text(&self, text: &str) {
        self.emit(json!({"type":"extension_ui_request", "id":pi_coding_agent::session::ids::uuid_v4(), "method":"set_editor_text", "text":text}));
    }

    async fn custom(&self, request: CustomRequest) -> Result<Option<Value>, String> {
        let id = pi_coding_agent::session::ids::uuid_v4();
        let mut component = request.factory.create_async().await?;
        let lines = pi_tui::Component::render(&mut component, 80);
        let component = Arc::new(Mutex::new(component));
        lock(&self.custom).insert(id.clone(), component);
        if let Some(handle_key) = request.handle_key {
            let _ = request.factory.invoke_callback(handle_key, json!(id)).await;
        }
        self.emit(json!({"type":"extension_ui_request", "id":id, "method":"custom", "lines":lines}));
        let result = request.done.await.ok();
        lock(&self.custom).remove(&id);
        self.emit(json!({"type":"extension_ui_request", "id":id, "method":"custom", "lines":[], "closed":true}));
        Ok(result)
    }
}
