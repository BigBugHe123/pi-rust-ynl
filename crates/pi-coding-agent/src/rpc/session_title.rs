use std::sync::Arc;
use std::time::Duration;

use crate::agent_session::AgentSession;
use pi_ai::{CacheRetention, Context, Message, StreamOptions, ThinkingLevel, UserContent, UserMessage};
use serde_json::{Value, json};

const USER_CHARS: usize = 800;
const ASSISTANT_CHARS: usize = 300;
const LAST_ASSISTANT_CHARS: usize = 600;
const SUMMARY_CHARS: usize = 600;
const TRANSCRIPT_CHARS: usize = 6000;
const TRANSCRIPT_HEAD_CHARS: usize = 2400;
const SYSTEM_PROMPT: &str = "You name chat sessions from a transcript. Reply with the title only.";
const TITLE_PROMPT: &str = "Create a concise title for this session based on the conversation above.\n\nRequirements:\n- Match the primary language used by the user.\n- Describe the user's concrete goal or the outcome, not the act of chatting.\n- Use 4-12 words for space-separated languages, or 8-24 characters for CJK text when practical.\n- Do not call any tools.\n- Return only the title as plain text, with no quotes, label, markdown, or explanation.";

fn text_of(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    let mut parts = Vec::new();
    let mut images = 0;
    for block in value.as_array().into_iter().flatten() {
        if block["type"] == "text" {
            if let Some(text) = block["text"].as_str() {
                parts.push(text.to_string());
            }
        }
        if block["type"] == "image" {
            images += 1;
        }
    }
    let mut text = parts.join("\n").trim().to_string();
    if images > 0 {
        let image = if images == 1 { "[image]".to_string() } else { format!("[image] x{images}") };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&image);
    }
    text
}

fn clip(value: &str, max: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max {
        value.to_string()
    } else {
        format!("{}{}", chars[..max].iter().collect::<String>(), '\u{2026}')
    }
}

fn message_line(message: &Value, last_assistant: bool) -> Option<String> {
    let role = message["role"].as_str().unwrap_or_default();
    if matches!(role, "compactionSummary" | "branchSummary") {
        let summary = message["summary"].as_str().unwrap_or_default().trim();
        return (!summary.is_empty()).then(|| format!("[Earlier summary] {}", clip(summary, SUMMARY_CHARS)));
    }
    if role == "bashExecution" {
        let command = message["command"].as_str().unwrap_or_default().trim();
        return (!command.is_empty()).then(|| format!("User: {}", clip(&format!("Ran `{command}`"), USER_CHARS)));
    }
    if role != "user" && role != "assistant" && role != "custom" {
        return None;
    }
    let text = text_of(&message["content"]).trim().to_string();
    if text.is_empty() {
        return None;
    }
    match role {
        "user" | "custom" => Some(format!("User: {}", clip(&text, USER_CHARS))),
        _ => Some(format!(
            "Assistant: {}",
            clip(&text, if last_assistant { LAST_ASSISTANT_CHARS } else { ASSISTANT_CHARS })
        )),
    }
}

fn bounded_transcript(messages: &[Value]) -> String {
    let last_assistant = messages
        .iter()
        .filter(|message| message["role"] == "assistant" && !text_of(&message["content"]).trim().is_empty())
        .last();
    let lines = messages
        .iter()
        .filter_map(|message| message_line(message, last_assistant == Some(message)))
        .collect::<Vec<_>>();
    let joined = lines.join("\n\n");
    if joined.chars().count() <= TRANSCRIPT_CHARS || lines.len() < 2 {
        return joined;
    }
    let mut head = Vec::new();
    let mut used = 7usize;
    let mut next = 0;
    for (index, line) in lines.iter().enumerate() {
        let size = used + line.chars().count() + 2;
        if !head.is_empty() && size > TRANSCRIPT_HEAD_CHARS {
            break;
        }
        head.push(line.clone());
        used = size;
        next = index + 1;
    }
    let mut tail = Vec::new();
    for line in lines[next..].iter().rev() {
        let size = used + line.chars().count() + 2;
        if size > TRANSCRIPT_CHARS {
            break;
        }
        tail.insert(0, line.clone());
        used = size;
    }
    if next + tail.len() >= lines.len() {
        return joined;
    }
    head.push("[\u{2026}]".to_string());
    head.extend(tail);
    head.join("\n\n")
}

fn parse_title(raw: &str) -> Result<String, String> {
    let mut value = raw.trim().to_string();
    if value.starts_with("```") && value.ends_with("```") {
        let inner = value.trim_start_matches("```").trim_end_matches("```").trim();
        value = inner.split_once('\n').map_or(inner, |(_, body)| body.trim()).to_string();
    }
    if value.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(&value) {
            if let Some(title) = parsed["title"].as_str() {
                value = title.trim().to_string();
            }
        }
    }
    value = value.lines().next().unwrap_or_default().trim().to_string();
    for prefix in [
        "session title:",
        "session title\u{ff1a}",
        "title:",
        "title\u{ff1a}",
        "\u{6807}\u{9898}:",
        "\u{6807}\u{9898}\u{ff1a}",
    ] {
        if value.to_lowercase().starts_with(prefix) {
            value = value[prefix.len()..].trim().to_string();
            break;
        }
    }
    for (start, end) in [
        ("\"", "\""),
        ("'", "'"),
        ("`", "`"),
        ("\u{201c}", "\u{201d}"),
        ("\u{300c}", "\u{300d}"),
        ("\u{300e}", "\u{300f}"),
    ] {
        if value.starts_with(start)
            && value.ends_with(end)
            && value.chars().count() > start.chars().count() + end.chars().count()
        {
            value = value[start.len()..value.len() - end.len()].trim().to_string();
            break;
        }
    }
    value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    while value.chars().last().is_some_and(|ch| ".!?\u{3002}\u{ff01}\u{ff1f}".contains(ch)) {
        value.pop();
    }
    if !value.chars().any(char::is_alphanumeric) {
        return Err("The model did not return a usable session title".into());
    }
    value = value.chars().take(80).collect::<String>().trim().to_string();
    Ok(value)
}

pub async fn generate(session: Arc<AgentSession>) -> Result<Value, String> {
    let messages = session
        .messages()
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let transcript = bounded_transcript(&messages);
    if transcript.trim().is_empty() {
        return Err("The session has no usable text to name".into());
    }
    let model = session.model().ok_or_else(|| "The session has no selected model".to_string())?;
    let runtime = session.model_runtime().clone();
    let auth = crate::agent_session::auth::request_auth(&runtime, &model).await?;
    let mut options = StreamOptions {
        session_id: Some(crate::session::ids::uuid_v4()),
        max_tokens: Some(256),
        cache_retention: Some(CacheRetention::None),
        timeout_ms: Some(90_000),
        ..StreamOptions::default()
    };
    if model.reasoning {
        let levels = pi_ai::simple_options::supported_thinking_levels(&model);
        let rejects_minimal = model.api == "google-generative-ai" || model.id.to_lowercase().contains("gemini");
        let level = levels
            .into_iter()
            .find(|level| !rejects_minimal || !matches!(level, ThinkingLevel::Off | ThinkingLevel::Minimal));
        if let Some(level) = level.filter(|level| *level != ThinkingLevel::Off) {
            options.reasoning = Some(level);
        }
    }
    crate::agent_session::auth::apply_request_settings(&mut options, &auth, session.settings());
    let context = Context {
        system_prompt: Some(SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![pi_ai::ContentBlock::text(format!("{transcript}\n\n{TITLE_PROMPT}"))]),
            timestamp: pi_ai::now_ms(),
        })],
        ..Context::default()
    };
    let response = tokio::time::timeout(Duration::from_millis(90_000), pi_ai::complete(&auth.model, &context, options))
        .await
        .map_err(|_| "Session title generation timed out".to_string())?;
    if response.stop_reason == pi_ai::StopReason::Error {
        return Err(response.error_message.unwrap_or_else(|| "The title model request failed".into()));
    }
    if response.stop_reason == pi_ai::StopReason::Aborted {
        return Err("Session title generation timed out".into());
    }
    let text = response.content.iter().filter_map(pi_ai::ContentBlock::as_text).collect::<Vec<_>>().join("\n");
    if text.trim().is_empty() {
        return Err("The model did not return a session title".into());
    }
    let title = parse_title(&text)?;
    Ok(
        json!({"title":title,"usage":{"input":response.usage.input,"output":response.usage.output,"cacheRead":response.usage.cache_read,"cacheWrite":response.usage.cache_write,"total":response.usage.total_tokens}}),
    )
}
