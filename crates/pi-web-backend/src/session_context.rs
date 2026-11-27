use serde_json::{Map, Value, json};

const MAX_TOOL_IMAGE_BYTES: usize = 10 * 1024 * 1024;

fn entry_shape(raw: &Value) -> Option<Value> {
    match raw["type"].as_str()? {
        "message" => {
            let role = raw["message"]["role"].as_str()?;
            (role != "system").then(|| json!({"role":role,"stopReason":raw["message"]["stopReason"],"display":true}))
        }
        "compaction" => Some(json!({"role":"custom","customType":"compaction","display":true})),
        "branch_summary" if !raw["summary"].as_str().unwrap_or_default().is_empty() => Some(json!({"role":"user"})),
        "custom_message" => Some(json!({"role":"custom","customType":raw["customType"],"display":raw["display"]})),
        _ => None,
    }
}

fn is_conversation(shape: &Value) -> bool {
    matches!(shape["role"].as_str(), Some("user" | "assistant" | "toolResult"))
}

fn is_hidden_custom(shape: &Value) -> bool {
    shape["role"] == "custom" && !shape["display"].as_bool().unwrap_or(false)
}

fn is_anchor(shape: &Value) -> bool {
    shape["role"] == "user"
        || (shape["role"] == "custom"
            && !is_hidden_custom(shape)
            && matches!(shape["customType"].as_str(), Some("compaction" | "subagent-notification")))
}

fn ends_run(shape: Option<&Value>) -> bool {
    shape.is_some_and(|shape| {
        shape["role"] == "assistant" && matches!(shape["stopReason"].as_str(), Some("stop" | "aborted"))
    })
}

fn turn_anchors(shapes: &[Value], open_tail: bool) -> Vec<bool> {
    let mut anchors = shapes.iter().map(is_anchor).collect::<Vec<_>>();
    let mut start = 0;
    while start < shapes.len() {
        if is_conversation(&shapes[start]) {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < shapes.len() && !is_conversation(&shapes[end]) {
            end += 1;
        }
        let answered =
            shapes.get(end).is_some_and(|shape| shape["role"] == "assistant") || (end == shapes.len() && open_tail);
        if answered
            && ends_run(start.checked_sub(1).and_then(|i| shapes.get(i)))
            && !anchors[start..end].contains(&true)
        {
            let mut candidate = None;
            for index in (start..end).rev() {
                if shapes[index]["role"] != "custom" {
                    if candidate.is_some() {
                        break;
                    }
                    continue;
                }
                if candidate.is_none() {
                    candidate = Some(index);
                }
                if !is_hidden_custom(&shapes[index]) {
                    candidate = Some(index);
                    break;
                }
            }
            if let Some(index) = candidate {
                anchors[index] = true;
            }
        }
        start = end;
    }
    anchors
}

fn parse_timestamp(value: &Value) -> Value {
    value.as_str().and_then(pi_coding_agent::session::parse_iso).map_or(Value::Null, |ms| json!(ms))
}

fn normalize_message(
    message: &Value,
    defer_media: bool,
    defer_thinking: bool,
    session_id: &str,
    entry_id: &str,
) -> Value {
    let mut message = message.clone();
    if message["role"] == "assistant" {
        if let Some(blocks) = message["content"].as_array_mut() {
            for block in blocks {
                if let Some(object) = block.as_object_mut() {
                    for key in ["thinkingSignature", "textSignature", "thoughtSignature"] {
                        object.remove(key);
                    }
                    if object.get("type").and_then(Value::as_str) == Some("toolCall") {
                        let call_id =
                            object.get("toolCallId").or_else(|| object.get("id")).cloned().unwrap_or_else(|| json!(""));
                        let name =
                            object.get("toolName").or_else(|| object.get("name")).cloned().unwrap_or_else(|| json!(""));
                        let input = object
                            .get("input")
                            .filter(|value| value.is_object())
                            .or_else(|| object.get("arguments").filter(|value| value.is_object()))
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        *object = Map::from_iter([
                            ("type".into(), json!("toolCall")),
                            ("toolCallId".into(), call_id),
                            ("toolName".into(), name),
                            ("input".into(), input),
                        ]);
                    }
                    if defer_thinking && object.get("type").and_then(Value::as_str) == Some("thinking") {
                        if let Some(text) = object.get("thinking").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                let preview = text
                                    .trim_start()
                                    .lines()
                                    .next()
                                    .unwrap_or_default()
                                    .chars()
                                    .take(240)
                                    .collect::<String>()
                                    .trim_end()
                                    .to_string();
                                object.insert("thinking".into(), json!(preview));
                                object.insert("deferred".into(), json!(true));
                            }
                        }
                    }
                }
            }
        } else if message["content"].is_string() {
            message["content"] = json!([{"type":"text","text":message["content"]}]);
        }
    }
    if defer_media && message["role"] == "toolResult" {
        let Some(blocks) = message["content"].as_array_mut() else {
            return message;
        };
        let mut out = Vec::new();
        let (mut omitted, mut bytes) = (0_usize, 0_usize);
        let mut mimes = Vec::<String>::new();
        for (index, block) in blocks.iter().enumerate() {
            let image = if block["type"] == "image" {
                if block["data"].is_string() {
                    Some((block["data"].as_str().unwrap_or_default(), block["mimeType"].as_str().unwrap_or_default()))
                } else if block["source"]["type"] == "base64" {
                    Some((
                        block["source"]["data"].as_str().unwrap_or_default(),
                        block["source"]["media_type"].as_str().unwrap_or_default(),
                    ))
                } else {
                    None
                }
            } else {
                None
            };
            let Some((encoded, mime)) = image else {
                out.push(block.clone());
                continue;
            };
            let pad =
                encoded.ends_with("==") as usize * 2 + (encoded.ends_with('=') && !encoded.ends_with("==")) as usize;
            let image_bytes = encoded.len().saturating_mul(3) / 4;
            let image_bytes = image_bytes.saturating_sub(pad);
            let allowed =
                matches!(mime, "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "image/bmp" | "image/avif");
            if allowed && image_bytes > 0 && image_bytes <= MAX_TOOL_IMAGE_BYTES {
                out.push(json!({"type":"image","source":{"type":"url","media_type":mime,"url":format!("/api/sessions/{session_id}/entries/{entry_id}/tool-result-image?blockIndex={index}")}}));
            } else {
                omitted += 1;
                bytes += image_bytes;
                if !mime.is_empty() && !mimes.contains(&mime.to_string()) {
                    mimes.push(mime.to_string());
                }
            }
        }
        if omitted > 0 {
            let mime_note = if mimes.is_empty() { String::new() } else { format!(": {}", mimes.join(", ")) };
            out.push(json!({"type":"text","text":format!("[{omitted} tool result image{} omitted from initial history payload{mime_note}, ~{bytes} bytes]",if omitted == 1 {""} else {"s"})}));
        }
        *blocks = out;
    }
    message
}

fn to_ui_message(raw: &Value, defer_thinking: bool, defer_media: bool, session_id: &str) -> Option<Value> {
    let entry_id = raw["id"].as_str().unwrap_or_default();
    match raw["type"].as_str()? {
        "message" => {
            let normalized = raw["message"].clone();
            if normalized["role"] == "system" {
                return None;
            }
            Some(normalize_message(&normalized, defer_media, defer_thinking, session_id, entry_id))
        }
        "compaction" => Some(
            json!({"role":"custom","customType":"compaction","content":raw["summary"],"display":true,"details":{"tokensBefore":raw["tokensBefore"],"firstKeptEntryId":raw["firstKeptEntryId"]},"timestamp":parse_timestamp(&raw["timestamp"])}),
        ),
        "branch_summary" => {
            let summary = raw["summary"].as_str().unwrap_or_default();
            (!summary.is_empty()).then(|| json!({"role":"user","content":format!("*The conversation briefly explored another branch and returned with this summary:*\n\n{summary}"),"timestamp":parse_timestamp(&raw["timestamp"])}))
        }
        "custom_message" => Some(
            json!({"role":"custom","customType":raw["customType"],"content":raw["content"],"display":raw["display"],"details":raw.get("details"),"timestamp":parse_timestamp(&raw["timestamp"])}),
        ),
        _ => None,
    }
}

pub fn build(
    entries: &[Value],
    current_leaf: Option<&str>,
    session_id: &str,
    leaf_query: Option<&str>,
    before: Option<&str>,
    tail: usize,
    defer_thinking: bool,
    defer_media: bool,
) -> Value {
    let leaf_query = leaf_query.filter(|leaf| !leaf.is_empty());
    let by_id = entries
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(|id| (id, entry)))
        .collect::<std::collections::HashMap<_, _>>();
    let branch_leaf = if let Some(before) = before {
        by_id.get(before).and_then(|entry| entry["parentId"].as_str())
    } else {
        leaf_query.or(current_leaf)
    };
    let mut branch = Vec::<&Value>::new();
    let mut current = branch_leaf;
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id) {
            break;
        }
        let Some(entry) = by_id.get(id).copied() else { break };
        branch.push(entry);
        current = entry["parentId"].as_str();
    }
    branch.reverse();
    let raw_cap = 200_usize.max(tail.saturating_mul(6));
    let mut start = branch.len();
    let mut visible = 0;
    while start > 0 && branch.len() - (start - 1) <= raw_cap {
        let entry = branch[start - 1];
        if entry["type"] == "compaction"
            || (entry["type"] == "message" && matches!(entry["message"]["role"].as_str(), Some("user" | "assistant")))
        {
            visible += 1;
        }
        start -= 1;
        if visible >= tail {
            break;
        }
    }
    let page = &branch[start..];
    let shapes_all = branch.iter().filter_map(|entry| entry_shape(entry)).collect::<Vec<_>>();
    let shapes_page = page.iter().filter_map(|entry| entry_shape(entry)).collect::<Vec<_>>();
    let all_anchors = turn_anchors(&shapes_all, false);
    let page_anchors = turn_anchors(&shapes_page, false);
    let page_shape_offset = branch[..start].iter().filter(|entry| entry_shape(entry).is_some()).count();
    let turns_before = all_anchors
        .iter()
        .enumerate()
        .filter(|(i, anchor)| {
            **anchor
                && (*i < page_shape_offset
                    || !page_anchors.get(i.saturating_sub(page_shape_offset)).copied().unwrap_or(false))
        })
        .count();
    let messages_and_ids = page
        .iter()
        .filter_map(|entry| {
            to_ui_message(entry, defer_thinking, defer_media, session_id)
                .map(|message| (message, entry["id"].as_str().unwrap_or_default().to_string()))
        })
        .collect::<Vec<_>>();
    let settings_leaf = if let Some(before) = before { Some(before) } else { leaf_query.or(current_leaf) };
    let mut settings_branch = Vec::<&Value>::new();
    let mut current = settings_leaf;
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id) {
            break;
        }
        let Some(entry) = by_id.get(id).copied() else { break };
        settings_branch.push(entry);
        current = entry["parentId"].as_str();
    }
    settings_branch.reverse();
    let (mut model_change, mut assistant_model, mut thinking) = (None, None, None);
    for entry in settings_branch {
        match entry["type"].as_str() {
            Some("model_change") => {
                model_change = Some(json!({"provider":entry["provider"],"modelId":entry["modelId"]}))
            }
            Some("thinking_level_change") => thinking = entry["thinkingLevel"].as_str().map(str::to_string),
            Some("message") if entry["message"]["role"] == "assistant" => {
                assistant_model =
                    Some(json!({"provider":entry["message"]["provider"],"modelId":entry["message"]["model"]}))
            }
            _ => {}
        }
    }
    let has_more = page.first().is_some_and(|entry| entry["parentId"].is_string());
    let leaf_id = leaf_query.map(str::to_string).or_else(|| current_leaf.map(str::to_string));
    json!({
        "context":{
            "messages":messages_and_ids.iter().map(|(message,_)|message.clone()).collect::<Vec<_>>(),
            "entryIds":messages_and_ids.iter().map(|(_,id)|id.clone()).collect::<Vec<_>>(),
            "oldestEntryId":page.first().and_then(|entry|entry["id"].as_str()),
            "hasMore":has_more,
            "turnsBefore":turns_before,
            "thinkingLevel":thinking.unwrap_or_else(||"off".into()),
            "model":model_change.or(assistant_model)
        },
        "tail":tail,
        "before":before,
        "leafId":leaf_id
    })
}
