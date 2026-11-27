use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::time::{Duration, Instant};

use pi_coding_agent::session::{SessionInfo, format_iso};
use serde_json::{Value, json};

const MAX_FILES: usize = 500;
const MAX_RESULTS: usize = 30;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const TIME_BUDGET: Duration = Duration::from_secs(3);
const SIDE_BOUNDARY: &str = "pi-web:side-boundary";

fn parse_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let (mut current, mut quoted) = (String::new(), false);
    for ch in query.chars() {
        if ch == '"' && (quoted || current.is_empty()) {
            if quoted && !current.is_empty() {
                terms.push(std::mem::take(&mut current));
            }
            quoted = !quoted;
        } else if !quoted && ch.is_whitespace() {
            if !current.is_empty() {
                terms.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    if terms.is_empty() && !query.is_empty() {
        terms.push(query.to_string());
    }
    let mut seen = HashSet::new();
    terms.into_iter().filter(|term| seen.insert(term.to_lowercase())).collect()
}

fn find_case_insensitive(text: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    let needle_lower = needle.to_lowercase();
    let start = lower.find(&needle_lower)?;
    // Lowercasing can expand a few Unicode scalars. Map its byte offsets back by
    // counting the prefix in the original text for the ordinary one-to-one case.
    let prefix_chars = lower[..start].chars().count();
    let match_chars = needle_lower.chars().count();
    let original: Vec<char> = text.chars().collect();
    if prefix_chars > original.len() {
        return None;
    }
    let start_byte = text.char_indices().nth(prefix_chars).map_or(text.len(), |(offset, _)| offset);
    let end_char = (prefix_chars + match_chars).min(original.len());
    let end_byte = text.char_indices().nth(end_char).map_or(text.len(), |(offset, _)| offset);
    Some((start_byte, end_byte))
}

fn compact_space(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn snippet(text: &str, start: usize, end: usize) -> (String, String, String) {
    let start = start.min(text.len());
    let end = end.min(text.len()).max(start);
    let before_text = &text[..start];
    let before_chars: Vec<char> = before_text.chars().collect();
    let before = before_chars[before_chars.len().saturating_sub(80)..].iter().collect::<String>();
    let after_text = &text[end..];
    let after_chars: Vec<char> = after_text.chars().collect();
    let after = after_chars.iter().take(80).collect::<String>();
    let before = format!("{}{}", if before_chars.len() > 80 { "..." } else { "" }, compact_space(&before));
    let after = format!("{}{}", compact_space(&after), if after_chars.len() > 80 { "..." } else { "" });
    (before, text[start..end].to_string(), after)
}

fn session_value(info: &SessionInfo, relation: Option<Value>, parent_id: Option<&str>) -> Value {
    let mut value = json!({
        "path":info.path,
        "id":info.id,
        "cwd":info.cwd,
        "created":format_iso(info.created),
        "modified":format_iso(info.modified),
        "messageCount":info.message_count,
        "firstMessage":info.first_message,
        "transient":false
    });
    if let Some(name) = &info.name {
        value["name"] = json!(name);
    }
    if let Some(parent_id) = parent_id {
        value["parentSessionId"] = json!(parent_id);
    }
    if let Some(relation) = relation {
        value["relation"] = relation;
    }
    value
}

fn relation_from_file(info: &SessionInfo) -> (Option<Value>, Option<String>, bool) {
    let Ok(file) = File::open(&info.path) else { return (None, None, false) };
    let reader = BufReader::new(file.take(256 * 1024));
    let mut relation = None;
    let mut parent = None;
    let mut is_side = false;
    for line in reader.lines().take(96).flatten().skip(1) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else { continue };
        let kind = entry["customType"].as_str().unwrap_or_default();
        let data = &entry["data"];
        if kind == "pi-web:side" {
            is_side = true;
            parent = data["parentSessionId"].as_str().map(str::to_string);
            relation = parent.as_deref().map(|id| json!({"kind":"side","parentSessionId":id}));
            break;
        }
    }
    if relation.is_none() {
        if info.parent_session_path.is_some() {
            relation = Some(json!({"kind":"fork"}));
        }
    }
    (relation, parent, is_side)
}

pub fn search(sessions: &[SessionInfo], query: &str) -> Value {
    let query = query.trim();
    if query.is_empty() {
        return json!({"results":[],"truncated":false});
    }
    let terms = parse_terms(query);
    let deadline = Instant::now() + TIME_BUDGET;
    let ids: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    let path_to_id = sessions
        .iter()
        .map(|session| (session.path.clone(), session.id.as_str()))
        .collect::<std::collections::HashMap<_, _>>();
    let relations: Vec<_> = sessions.iter().map(relation_from_file).collect();
    let mut order = (0..sessions.len()).collect::<Vec<_>>();
    order.sort_by(|a, b| sessions[*b].modified.cmp(&sessions[*a].modified));
    let mut results = Vec::new();
    let mut truncated = false;
    let mut scanned_files = 0;

    for index in order {
        if scanned_files >= MAX_FILES || results.len() >= MAX_RESULTS || Instant::now() >= deadline {
            truncated = true;
            break;
        }
        let info = &sessions[index];
        let (relation, relation_parent, is_side) = &relations[index];
        let parent_id = relation_parent.as_deref().or_else(|| {
            info.parent_session_path.as_ref().and_then(|path| path_to_id.get(std::path::Path::new(path)).copied())
        });
        let mut relation_value = relation.clone();
        if relation_value.as_ref().is_some_and(|value| value["kind"] == "fork") {
            if let Some(parent_id) = parent_id {
                relation_value.as_mut().unwrap()["originSessionId"] = json!(parent_id);
            }
        }
        if *is_side && !relation_parent.as_deref().is_some_and(|parent| ids.contains(parent)) {
            continue;
        }
        scanned_files += 1;
        let Ok(file) = File::open(&info.path) else {
            truncated = true;
            continue;
        };
        let mut reader = BufReader::new(file.take(MAX_FILE_BYTES));
        let mut bytes_read = 0_u64;
        let mut line = Vec::new();
        let mut past_boundary = !*is_side;
        let mut hit: Option<Value> = None;
        let mut pending = terms.iter().skip(1).map(|term| term.to_lowercase()).collect::<HashSet<_>>();
        let first_term = terms.first().map(|term| term.to_lowercase()).unwrap_or_default();
        loop {
            line.clear();
            let count = match reader.read_until(b'\n', &mut line) {
                Ok(count) => count,
                Err(_) => {
                    truncated = true;
                    break;
                }
            };
            if count == 0 {
                break;
            }
            bytes_read += count as u64;
            if Instant::now() >= deadline {
                truncated = true;
                break;
            }
            if line.len() > MAX_LINE_BYTES {
                truncated = true;
                continue;
            }
            let line_text = String::from_utf8_lossy(&line);
            if !past_boundary && !line_text.contains(SIDE_BOUNDARY) {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(&line_text) else { continue };
            if entry["type"] == "custom_message" && entry["customType"] == SIDE_BOUNDARY {
                past_boundary = true;
                continue;
            }
            if !past_boundary
                || entry["type"] != "message"
                || !matches!(entry["message"]["role"].as_str(), Some("user" | "assistant"))
            {
                continue;
            }
            let content = &entry["message"]["content"];
            let blocks: Vec<(usize, String)> = if let Some(text) = content.as_str() {
                vec![(0, text.to_string())]
            } else {
                content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter_map(|(i, block)| {
                        (block["type"] == "text")
                            .then(|| block["text"].as_str().map(|text| (i, text.to_string())))
                            .flatten()
                    })
                    .collect()
            };
            let joined = blocks.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>().join("\n");
            let joined_lower = joined.to_lowercase();
            pending.retain(|term| !joined_lower.contains(term));
            if hit.is_none() {
                if let Some((start, end)) = find_case_insensitive(&joined, &first_term) {
                    let mut offset = 0;
                    let block_index = blocks
                        .iter()
                        .find_map(|(block_index, text)| {
                            let block_start = offset;
                            let block_end = block_start + text.len();
                            offset = block_end + 1;
                            (start <= block_end).then_some(*block_index)
                        })
                        .unwrap_or(0);
                    let (before, matched, after) = snippet(&joined, start, end);
                    let mut item = json!({"session":session_value(info, relation_value.clone(), parent_id),"blockIndex":block_index,"before":before,"match":compact_space(&matched),"after":after});
                    if let Some(entry_id) = entry["id"].as_str() {
                        item["entryId"] = json!(entry_id);
                    }
                    hit = Some(item);
                }
            }
            if hit.is_some() && pending.is_empty() {
                break;
            }
        }
        if bytes_read >= MAX_FILE_BYTES {
            truncated = true;
        }
        if let Some(hit) = hit {
            if pending.is_empty() {
                results.push(hit);
                continue;
            }
        }
        if !past_boundary || Instant::now() >= deadline {
            continue;
        }
        let title = if *is_side {
            info.name.as_deref().filter(|name| *name != "Side chat").unwrap_or("")
        } else {
            info.name.as_deref().unwrap_or(&info.first_message)
        };
        let title_lower = title.to_lowercase();
        if terms.iter().all(|term| title_lower.contains(&term.to_lowercase())) {
            if let Some((start, end)) = find_case_insensitive(title, &first_term) {
                let (before, matched, after) = snippet(title, start, end);
                results.push(json!({
                    "session":session_value(info, relation_value, parent_id),
                    "blockIndex":0,"titleMatch":true,
                    "before":before,"match":matched,"after":after
                }));
            }
        }
    }
    json!({"results":results,"truncated":truncated})
}
