use serde_json::Value;

const CODING_TOOLS: &[&str] = &["read", "bash", "powershell", "edit", "write", "grep", "find", "ls"];
const SUBAGENT_TOOLS: &[&str] = &["Agent", "get_subagent_result", "steer_subagent"];

fn is_path_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '~' | '+' | '%' | '@' | '/' | '\\' | ':' | '-')
}

fn decode_uri_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = ((bytes[i + 1] as char).to_digit(16), (bytes[i + 2] as char).to_digit(16))
            {
                decoded.push(((high << 4) | low) as u8);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn contains_reference(text: &str, file_path: &str) -> bool {
    let target = file_path.replace('\\', "/");
    let mut targets = vec![target.clone()];
    if target.starts_with('/') {
        targets.push(format!("file://{target}"));
    }
    let decoded = decode_uri_component(text).replace('\\', "/");
    let normalized = text.replace('\\', "/");
    let haystacks = [&normalized, &decoded];
    for haystack in haystacks {
        let chars = haystack.chars().collect::<Vec<_>>();
        for candidate in &targets {
            let needle = candidate.chars().collect::<Vec<_>>();
            if needle.is_empty() || needle.len() > chars.len() {
                continue;
            }
            for start in 0..=chars.len() - needle.len() {
                if chars[start..start + needle.len()] != needle {
                    continue;
                }
                let before_ok = start == 0 || !is_path_char(chars[start - 1]);
                let after = start + needle.len();
                let after_ok = if after == chars.len() {
                    true
                } else if chars[after] == ':' {
                    chars.get(after + 1).is_some_and(|ch| ch.is_ascii_digit())
                } else {
                    !is_path_char(chars[after])
                };
                if before_ok && after_ok {
                    return true;
                }
            }
        }
    }
    false
}

fn collect_strings(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(text) => output.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, output)),
        Value::Object(object) => object.values().for_each(|item| collect_strings(item, output)),
        _ => {}
    }
}

fn entry_strings(entry: &Value) -> Vec<String> {
    if entry["type"].as_str() == Some("custom") && entry["customType"].as_str() == Some("codemode-store") {
        return Vec::new();
    }
    if entry["type"].as_str() == Some("context_edit") {
        return Vec::new();
    }
    if entry["type"].as_str() == Some("message") {
        let message = &entry["message"];
        if message["role"].as_str() == Some("system") {
            return Vec::new();
        }
        if message["role"].as_str() == Some("toolResult") {
            let tool_name = message["toolName"].as_str().unwrap_or_default();
            if !CODING_TOOLS.contains(&tool_name) && !SUBAGENT_TOOLS.contains(&tool_name) {
                let mut output = Vec::new();
                if let Some(path) = message["details"]["fullOutputPath"].as_str() {
                    output.push(path.to_string());
                }
                if let Some(calls) = message["nestedCalls"]["calls"].as_array() {
                    for call in calls {
                        if call["name"].as_str().is_some_and(|name| CODING_TOOLS.contains(&name)) {
                            collect_strings(&call["arguments"], &mut output);
                        }
                    }
                }
                return output;
            }
        }
    }
    let mut output = Vec::new();
    collect_strings(entry, &mut output);
    output
}

pub fn is_file_path_referenced(entries: &[Value], file_path: &str) -> bool {
    entries.iter().any(|entry| entry_strings(entry).iter().any(|text| contains_reference(text, file_path)))
}

pub fn is_bash_output_path_referenced(entries: &[Value], file_path: &str) -> bool {
    entries.iter().any(|entry| {
        entry["type"].as_str() == Some("message")
            && entry["message"]["role"].as_str() == Some("bashExecution")
            && entry["message"]["fullOutputPath"].as_str() == Some(file_path)
    })
}
