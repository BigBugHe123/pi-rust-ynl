use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::app_state::lock;

#[derive(Default)]
pub(crate) struct WebExtensionUi {
    pending: Mutex<BTreeMap<String, Value>>,
    statuses: Mutex<BTreeMap<String, Value>>,
    widgets: Mutex<BTreeMap<String, Value>>,
}

impl WebExtensionUi {
    pub(crate) fn observe(&self, event: &Value) {
        if event["type"] == "extension_ui_closed" {
            if let Some(id) = event["id"].as_str() {
                self.dismiss(id);
            }
            return;
        }
        if event["type"] != "extension_ui_request" {
            return;
        }
        let Some(id) = event["id"].as_str() else { return };
        match event["method"].as_str() {
            Some("select" | "confirm" | "input" | "editor" | "custom") => {
                if event["closed"] == true {
                    self.dismiss(id);
                } else {
                    lock(&self.pending).insert(id.into(), event.clone());
                }
            }
            Some("setStatus") => {
                if let Some(key) = event["statusKey"].as_str() {
                    let mut statuses = lock(&self.statuses);
                    if let Some(text) = event["statusText"].as_str() {
                        statuses.insert(key.into(), json!({"key":key,"text":text}));
                    } else {
                        statuses.remove(key);
                    }
                }
            }
            Some("setWidget") => {
                if let Some(key) = event["widgetKey"].as_str() {
                    let mut widgets = lock(&self.widgets);
                    if let Some(lines) = event["widgetLines"].as_array() {
                        widgets.insert(key.into(), json!({"key":key,"lines":lines,"placement":event["widgetPlacement"].as_str().unwrap_or("aboveEditor")}));
                    } else {
                        widgets.remove(key);
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn dismiss(&self, id: &str) {
        lock(&self.pending).remove(id);
    }
    pub(crate) fn pending_requests(&self) -> Vec<Value> {
        lock(&self.pending).values().cloned().collect()
    }
    pub(crate) fn awaiting_input(&self) -> bool {
        !lock(&self.pending).is_empty()
    }
    fn has_widget(&self, key: &str) -> bool {
        lock(&self.widgets)
            .get(key)
            .and_then(|widget| widget["lines"].as_array())
            .is_some_and(|lines| !lines.is_empty())
    }

    pub(crate) fn has_subagents(&self) -> bool {
        self.has_widget("subagents")
    }

    pub(crate) fn has_background_work(&self) -> bool {
        self.has_widget("background-tasks") || self.has_subagents()
    }
    pub(crate) fn statuses(&self) -> Vec<Value> {
        lock(&self.statuses).values().cloned().collect()
    }
    pub(crate) fn widgets(&self) -> Vec<Value> {
        lock(&self.widgets).values().cloned().collect()
    }
}
