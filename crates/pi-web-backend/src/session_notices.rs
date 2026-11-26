use std::collections::HashMap;
use std::fs;
use std::path::Path;

use pi_coding_agent::cache_stats::{self, ModelPriceSource};
use pi_coding_agent::session::{Entry, SessionManager};
use serde_json::{Value, json};

#[derive(Default)]
struct SessionPrices(HashMap<(String, String), f64>);

impl ModelPriceSource for SessionPrices {
    fn cache_read_price(&self, provider: &str, model: &str) -> Option<f64> {
        self.0.get(&(provider.to_string(), model.to_string())).copied()
    }
}

fn setting(path: &Path) -> Option<bool> {
    let bytes = fs::read(path).ok()?;
    let content = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let value: Value = serde_json::from_slice(content).ok()?;
    value.get("showCacheMissNotices")?.as_bool()
}

fn enabled(cwd: &str, agent_dir: &Path) -> bool {
    if cwd.is_empty() {
        return false;
    }
    setting(&Path::new(cwd).join(".pi/settings.json"))
        .or_else(|| setting(&agent_dir.join("settings.json")))
        .unwrap_or(false)
}

fn thinking_dropped(message: &Value) -> usize {
    message["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|diagnostic| diagnostic["type"] == "anthropic_input_transformations")
        .map(|diagnostic| {
            diagnostic["details"]["transformations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "thinking_dropped")
                .count()
        })
        .sum()
}

fn standalone(entry: &Entry) -> bool {
    let raw = entry.raw();
    match entry.kind() {
        "message" => !matches!(
            raw.get("message").and_then(|m| m.get("role")).and_then(Value::as_str),
            Some("system" | "toolResult")
        ),
        "compaction" => true,
        "branch_summary" => raw.get("summary").and_then(Value::as_str).is_some_and(|s| !s.is_empty()),
        "custom_message" => raw.get("display").and_then(Value::as_bool) == Some(true),
        _ => false,
    }
}

pub fn attach(manager: &SessionManager, context: &mut Value, cwd: &str, agent_dir: &Path) {
    if !enabled(cwd, agent_dir) {
        return;
    }
    let entries = manager.entries();
    let mut prices = SessionPrices::default();
    for entry in entries {
        let raw = entry.raw();
        if raw.get("type").and_then(Value::as_str) != Some("message") || raw["message"]["role"] != "assistant" {
            continue;
        }
        let usage = &raw["message"]["usage"];
        let reads = usage["cacheRead"].as_f64().unwrap_or(0.0);
        let cost = usage["cost"]["cacheRead"].as_f64().unwrap_or(0.0);
        if reads > 0.0 {
            let key = (
                raw["message"]["provider"].as_str().unwrap_or_default().to_string(),
                raw["message"]["model"].as_str().unwrap_or_default().to_string(),
            );
            prices.0.insert(key, cost / reads * 1_000_000.0);
        }
    }
    let misses = cache_stats::collect_cache_misses(entries, &prices);
    let by_id: HashMap<&str, &Entry> = entries.iter().map(|entry| (entry.id(), entry)).collect();
    let mut notices: HashMap<String, Vec<Value>> = HashMap::new();
    let mut warm_lines: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut branch = std::collections::HashSet::new();
    let mut current = manager.leaf_id();
    while let Some(id) = current {
        if !branch.insert(id) {
            break;
        }
        current = by_id.get(id).and_then(|entry| entry.parent_id());
    }

    for entry in entries {
        let raw = entry.raw();
        match entry.kind() {
            "message"
                if raw["message"]["role"] == "assistant"
                    && !matches!(raw["message"]["stopReason"].as_str(), Some("aborted" | "error")) =>
            {
                let list = notices.entry(entry.id().to_string()).or_default();
                if let Some(miss) = misses.get(entry.id()).filter(|miss| cache_stats::cache_miss_notice(miss).is_some())
                {
                    let mut notice =
                        json!({"kind":"cache_miss","missedTokens":miss.missed_tokens,"missedCost":miss.missed_cost});
                    if miss.model_changed {
                        notice["modelChanged"] = json!(true);
                    } else if miss.idle_ms >= cache_stats::CACHE_TTL_MS {
                        notice["idleMinutes"] = json!((miss.idle_ms as f64 / 60_000.0).round());
                    }
                    list.push(notice);
                }
                let dropped = thinking_dropped(&raw["message"]);
                if dropped > 0 {
                    let mut ancestor = entry.parent_id().and_then(|id| by_id.get(id).copied());
                    let mut previous = 0;
                    while let Some(candidate) = ancestor {
                        if candidate.kind() == "message" && candidate.raw()["message"]["role"] == "assistant" {
                            previous = thinking_dropped(&candidate.raw()["message"]);
                            break;
                        }
                        ancestor = candidate.parent_id().and_then(|id| by_id.get(id).copied());
                    }
                    if dropped > previous {
                        list.push(json!({"kind":"thinking_dropped","count":dropped}));
                    }
                }
            }
            "compaction" | "branch_summary" => {
                let list = notices.entry(entry.id().to_string()).or_default();
                let summary = entry.kind() == "compaction" || raw["summary"].as_str().is_some_and(|s| !s.is_empty());
                let usage = &raw["usage"];
                if summary && usage.is_object() {
                    let tokens = ["input", "output", "cacheRead", "cacheWrite"]
                        .iter()
                        .map(|key| usage[key].as_u64().unwrap_or(0))
                        .sum::<u64>();
                    list.push(json!({"kind":"summary_cost","source":entry.kind(),"tokens":tokens,"cost":usage["cost"]["total"].as_f64().unwrap_or(0.0)}));
                }
            }
            "usage" if raw["kind"] == "cache_warm" && branch.contains(entry.id()) => {
                let mut ancestor = entry.parent_id().and_then(|id| by_id.get(id).copied());
                while let Some(candidate) = ancestor {
                    if standalone(candidate) {
                        let note = raw["note"].as_str().filter(|note| !note.is_empty()).map(str::to_string);
                        let cost = raw["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
                        if let Some((previous_anchor, previous_note)) =
                            entry.parent_id().and_then(|id| warm_lines.get(id))
                            && previous_anchor == candidate.id()
                            && previous_note == &note
                            && let Some(previous) = notices.get_mut(candidate.id()).and_then(|items| items.last_mut())
                            && previous["kind"] == "cache_warm"
                            && previous["note"].as_str().map(str::to_string) == note
                        {
                            previous["cost"] = json!(previous["cost"].as_f64().unwrap_or(0.0) + cost);
                            warm_lines.insert(entry.id().to_string(), (candidate.id().to_string(), note));
                            break;
                        }
                        let mut notice = json!({"kind":"cache_warm","cost":cost});
                        if let Some(note) = &note {
                            notice["note"] = json!(note);
                        }
                        notices.entry(candidate.id().to_string()).or_default().push(notice);
                        warm_lines.insert(entry.id().to_string(), (candidate.id().to_string(), note));
                        break;
                    }
                    ancestor = candidate.parent_id().and_then(|id| by_id.get(id).copied());
                }
            }
            _ => {}
        }
    }
    let ids = context["entryIds"].as_array().cloned().unwrap_or_default();
    if let Some(messages) = context["messages"].as_array_mut() {
        for (index, id) in ids.iter().enumerate() {
            let Some(message) = messages.get_mut(index).and_then(Value::as_object_mut) else { continue };
            let Some(items) = id.as_str().and_then(|id| notices.get(id)).filter(|items| !items.is_empty()) else {
                continue;
            };
            message.insert("transcriptNotices".into(), json!(items));
        }
    }
}
