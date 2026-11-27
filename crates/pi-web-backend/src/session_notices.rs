use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

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

fn standalone(raw: &Value) -> bool {
    match raw["type"].as_str() {
        Some("message") => !matches!(raw["message"]["role"].as_str(), Some("system" | "toolResult")),
        Some("compaction") => true,
        Some("branch_summary") => raw["summary"].as_str().is_some_and(|s| !s.is_empty()),
        Some("custom_message") => raw["display"].as_bool() == Some(true),
        _ => false,
    }
}

#[derive(Clone, Copy)]
struct CacheMiss {
    missed_tokens: u64,
    missed_cost: f64,
    idle_ms: i64,
    model_changed: bool,
}

fn cache_misses(entries: &[Value]) -> HashMap<String, CacheMiss> {
    let mut prices = HashMap::<(String, String), f64>::new();
    for entry in entries {
        let message = &entry["message"];
        if entry["type"] != "message" || message["role"] != "assistant" {
            continue;
        }
        let reads = message["usage"]["cacheRead"].as_f64().unwrap_or(0.0);
        let cost = message["usage"]["cost"]["cacheRead"].as_f64().unwrap_or(0.0);
        if reads > 0.0 {
            prices.insert(
                (
                    message["provider"].as_str().unwrap_or_default().into(),
                    message["model"].as_str().unwrap_or_default().into(),
                ),
                cost / reads * 1_000_000.0,
            );
        }
    }
    let mut last: Option<(u64, String, i64, bool)> = None;
    let mut misses = HashMap::new();
    for entry in entries {
        let kind = entry["type"].as_str().unwrap_or_default();
        if matches!(kind, "compaction" | "branch_summary") {
            last = None;
            continue;
        }
        if kind == "usage" && entry["kind"] == "cache_warm" {
            let usage = &entry["usage"];
            let tokens = usage["input"].as_u64().unwrap_or(0)
                + usage["cacheRead"].as_u64().unwrap_or(0)
                + usage["cacheWrite"].as_u64().unwrap_or(0);
            if tokens > 0 {
                let model_key = format!(
                    "{}/{}",
                    entry["provider"].as_str().unwrap_or_default(),
                    entry["model"].as_str().unwrap_or_default()
                );
                let timestamp = entry["timestamp"].as_str().and_then(pi_coding_agent::session::parse_iso).unwrap_or(0);
                last = Some((tokens, model_key, timestamp, true));
            }
            continue;
        }
        if kind != "message" || entry["message"]["role"] != "assistant" {
            continue;
        }
        let message = &entry["message"];
        let usage = &message["usage"];
        let input = usage["input"].as_u64().unwrap_or(0);
        let reads = usage["cacheRead"].as_u64().unwrap_or(0);
        let writes = usage["cacheWrite"].as_u64().unwrap_or(0);
        let tokens = input + reads + writes;
        if let Some((last_tokens, last_model, last_time, reported)) = last.as_ref() {
            if tokens > 0 && (reads + writes > 0 || *reported) {
                let missed = (*last_tokens).min(tokens).saturating_sub(reads);
                if missed > 1024 {
                    let paid = input + writes;
                    let paid_rate = if paid > 0 {
                        (usage["cost"]["input"].as_f64().unwrap_or(0.0)
                            + usage["cost"]["cacheWrite"].as_f64().unwrap_or(0.0))
                            / paid as f64
                    } else {
                        0.0
                    };
                    let read_rate = if reads > 0 {
                        usage["cost"]["cacheRead"].as_f64().unwrap_or(0.0) / reads as f64
                    } else {
                        prices
                            .get(&(
                                message["provider"].as_str().unwrap_or_default().into(),
                                message["model"].as_str().unwrap_or_default().into(),
                            ))
                            .copied()
                            .unwrap_or(0.0)
                            / 1_000_000.0
                    };
                    let time = message["timestamp"].as_i64().unwrap_or(0);
                    let model_key = format!(
                        "{}/{}",
                        message["provider"].as_str().unwrap_or_default(),
                        message["model"].as_str().unwrap_or_default()
                    );
                    misses.insert(
                        entry["id"].as_str().unwrap_or_default().to_string(),
                        CacheMiss {
                            missed_tokens: missed,
                            missed_cost: missed as f64 * (paid_rate - read_rate).max(0.0),
                            idle_ms: time.saturating_sub(*last_time).max(0),
                            model_changed: model_key != *last_model,
                        },
                    );
                }
            }
        }
        if tokens > 0 {
            let model_key = format!(
                "{}/{}",
                message["provider"].as_str().unwrap_or_default(),
                message["model"].as_str().unwrap_or_default()
            );
            let time = message["timestamp"].as_i64().unwrap_or(0);
            last = Some((tokens, model_key, time, last.as_ref().is_some_and(|last| last.3) || reads + writes > 0));
        }
    }
    misses
}

fn cache_miss_visible(miss: &CacheMiss) -> bool {
    miss.missed_tokens >= 20_000 || miss.missed_cost >= 0.1
}

pub fn attach(entries: &[Value], leaf_id: Option<&str>, context: &mut Value, cwd: &str, agent_dir: &Path) {
    if !enabled(cwd, agent_dir) {
        return;
    }
    let misses = cache_misses(entries);
    let by_id: HashMap<&str, &Value> =
        entries.iter().filter_map(|entry| entry["id"].as_str().map(|id| (id, entry))).collect();
    let mut notices: HashMap<String, Vec<Value>> = HashMap::new();
    let mut warm_lines: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut branch = std::collections::HashSet::new();
    let mut current = leaf_id;
    while let Some(id) = current {
        if !branch.insert(id.to_string()) {
            break;
        }
        current = by_id.get(id).and_then(|entry| entry["parentId"].as_str());
    }

    for entry in entries {
        let raw = entry;
        let entry_id = raw["id"].as_str().unwrap_or_default();
        match raw["type"].as_str().unwrap_or_default() {
            "message"
                if raw["message"]["role"] == "assistant"
                    && !matches!(raw["message"]["stopReason"].as_str(), Some("aborted" | "error")) =>
            {
                let list = notices.entry(entry_id.to_string()).or_default();
                if let Some(miss) = misses.get(entry_id).filter(|miss| cache_miss_visible(miss)) {
                    let mut notice =
                        json!({"kind":"cache_miss","missedTokens":miss.missed_tokens,"missedCost":miss.missed_cost});
                    if miss.model_changed {
                        notice["modelChanged"] = json!(true);
                    } else if miss.idle_ms >= CACHE_TTL_MS {
                        notice["idleMinutes"] = json!((miss.idle_ms as f64 / 60_000.0).round());
                    }
                    list.push(notice);
                }
                let dropped = thinking_dropped(&raw["message"]);
                if dropped > 0 {
                    let mut ancestor = raw["parentId"].as_str().and_then(|id| by_id.get(id).copied());
                    let mut previous = 0;
                    while let Some(candidate) = ancestor {
                        if candidate["type"] == "message" && candidate["message"]["role"] == "assistant" {
                            previous = thinking_dropped(&candidate["message"]);
                            break;
                        }
                        ancestor = candidate["parentId"].as_str().and_then(|id| by_id.get(id).copied());
                    }
                    if dropped > previous {
                        list.push(json!({"kind":"thinking_dropped","count":dropped}));
                    }
                }
            }
            "compaction" | "branch_summary" => {
                let list = notices.entry(entry_id.to_string()).or_default();
                let summary = raw["type"] == "compaction" || raw["summary"].as_str().is_some_and(|s| !s.is_empty());
                let usage = &raw["usage"];
                if summary && usage.is_object() {
                    let tokens = ["input", "output", "cacheRead", "cacheWrite"]
                        .iter()
                        .map(|key| usage[key].as_u64().unwrap_or(0))
                        .sum::<u64>();
                    list.push(json!({"kind":"summary_cost","source":raw["type"],"tokens":tokens,"cost":usage["cost"]["total"].as_f64().unwrap_or(0.0)}));
                }
            }
            "usage" if raw["kind"] == "cache_warm" && branch.contains(entry_id) => {
                let mut ancestor = raw["parentId"].as_str().and_then(|id| by_id.get(id).copied());
                while let Some(candidate) = ancestor {
                    if standalone(candidate) {
                        let note = raw["note"].as_str().filter(|note| !note.is_empty()).map(str::to_string);
                        let cost = raw["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
                        if let Some((previous_anchor, previous_note)) =
                            raw["parentId"].as_str().and_then(|id| warm_lines.get(id))
                            && previous_anchor == candidate["id"].as_str().unwrap_or_default()
                            && previous_note == &note
                            && let Some(previous) = notices
                                .get_mut(candidate["id"].as_str().unwrap_or_default())
                                .and_then(|items| items.last_mut())
                            && previous["kind"] == "cache_warm"
                            && previous["note"].as_str().map(str::to_string) == note
                        {
                            previous["cost"] = json!(previous["cost"].as_f64().unwrap_or(0.0) + cost);
                            warm_lines.insert(
                                entry_id.to_string(),
                                (candidate["id"].as_str().unwrap_or_default().to_string(), note),
                            );
                            break;
                        }
                        let mut notice = json!({"kind":"cache_warm","cost":cost});
                        if let Some(note) = &note {
                            notice["note"] = json!(note);
                        }
                        notices
                            .entry(candidate["id"].as_str().unwrap_or_default().to_string())
                            .or_default()
                            .push(notice);
                        warm_lines.insert(
                            entry_id.to_string(),
                            (candidate["id"].as_str().unwrap_or_default().to_string(), note),
                        );
                        break;
                    }
                    ancestor = candidate["parentId"].as_str().and_then(|id| by_id.get(id).copied());
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
