use std::collections::HashSet;

use pi_coding_agent::session::SessionTree;
use serde_json::{Value, json};

const MAX_DEPTH: usize = 200;
const PREVIEW_LENGTH: usize = 40;

fn append_preview_text(text: &mut String, value: &str) {
    if text.chars().count() > PREVIEW_LENGTH {
        return;
    }
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return;
    }
    let addition = format!("{}{}", if text.is_empty() { "" } else { " " }, normalized);
    let remaining = PREVIEW_LENGTH + 1usize.saturating_sub(text.chars().count());
    text.push_str(&addition.chars().take(remaining).collect::<String>());
}

fn preview(entry: &Value) -> Option<Value> {
    if entry["type"] != "message" || !entry["message"]["role"].is_string() || entry["message"]["role"] == "system" {
        return None;
    }
    let role = entry["message"]["role"].as_str().unwrap_or_default();
    let content = &entry["message"]["content"];
    let mut text = String::new();
    let mut image = false;
    if let Some(value) = content.as_str() {
        append_preview_text(&mut text, value);
    } else if let Some(blocks) = content.as_array() {
        for block in blocks {
            if block["type"] == "image" {
                image = true;
            }
            if block["type"] == "text" {
                if let Some(value) = block["text"].as_str() {
                    append_preview_text(&mut text, value);
                }
            }
            if text.chars().count() > PREVIEW_LENGTH {
                break;
            }
        }
    }
    if text.chars().count() > PREVIEW_LENGTH {
        text = format!("{}…", text.chars().take(PREVIEW_LENGTH).collect::<String>());
    } else if text.is_empty() {
        text = if image {
            "[image]"
        } else if role == "assistant" {
            "[assistant]"
        } else {
            "message"
        }
        .into();
    }
    if matches!(role, "user" | "assistant") {
        Some(json!({"role":role,"text":text}))
    } else {
        Some(json!({"text":text}))
    }
}

fn clean_entry(value: &Value) -> Value {
    if value["type"] != "message" || value["message"]["role"] != "assistant" {
        return value.clone();
    }
    let mut entry = value.clone();
    if let Some(blocks) = entry["message"]["content"].as_array_mut() {
        for block in blocks {
            if let Some(object) = block.as_object_mut() {
                for key in ["thinkingSignature", "textSignature", "thoughtSignature"] {
                    object.remove(key);
                }
            }
        }
    }
    entry
}

struct ProjectedNode {
    source: usize,
    children: Vec<usize>,
    compressed: Vec<String>,
    branch_preview: Option<Value>,
}

fn node_value(tree: &SessionTree<'_>, node: &ProjectedNode, children: Vec<Value>, summary: bool) -> Value {
    let source = tree.node(node.source);
    let raw = Value::Object(source.entry.raw().clone());
    let mut value = if summary {
        let parent = raw.get("parentId").cloned().unwrap_or(Value::Null);
        json!({"entry":{"id":raw["id"],"parentId":parent,"type":raw["type"],"timestamp":raw.get("timestamp").cloned().unwrap_or(json!(""))},"children":children})
    } else {
        let mut value = json!({"entry":clean_entry(&raw),"children":children});
        if let Some(label) = &source.label {
            value["label"] = json!(label);
        }
        if let Some(timestamp) = &source.label_timestamp {
            value["labelTimestamp"] = json!(timestamp);
        }
        value
    };
    if !node.compressed.is_empty() {
        value["compressedEntryIds"] = json!(node.compressed);
    }
    if let Some(preview) = &node.branch_preview {
        value["branchPreview"] = preview.clone();
    }
    value
}

pub fn build(tree: &SessionTree<'_>, summary: bool) -> Value {
    let roots_set: HashSet<usize> = tree.roots.iter().copied().collect();
    let keep: HashSet<usize> = tree
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| (roots_set.contains(&i) || node.children.len() != 1).then_some(i))
        .collect();
    let mut projected = Vec::<ProjectedNode>::new();
    let mut projected_roots = Vec::new();
    let mut tasks = Vec::new();
    for &root in &tree.roots {
        let index = projected.len();
        projected.push(ProjectedNode {
            source: root,
            children: Vec::new(),
            compressed: Vec::new(),
            branch_preview: preview(&Value::Object(tree.node(root).entry.raw().clone())),
        });
        projected_roots.push(index);
        tasks.push((root, index, 1usize));
    }
    while let Some((source, parent, depth)) = tasks.pop() {
        for &source_child in &tree.node(source).children {
            if depth >= MAX_DEPTH {
                let mut pending = vec![(source_child, Vec::<String>::new(), None::<Value>)];
                let mut seen = HashSet::new();
                while let Some((current, compressed, carried)) = pending.pop() {
                    if !seen.insert(current) {
                        continue;
                    }
                    let current_preview =
                        carried.or_else(|| preview(&Value::Object(tree.node(current).entry.raw().clone())));
                    let mut child_projected = None;
                    if keep.contains(&current) {
                        let index = projected.len();
                        projected.push(ProjectedNode {
                            source: current,
                            children: Vec::new(),
                            compressed: compressed.clone(),
                            branch_preview: current_preview.clone(),
                        });
                        projected[parent].children.push(index);
                        child_projected = Some(index);
                    }
                    for &child in tree.node(current).children.iter().rev() {
                        let mut next_compressed = compressed.clone();
                        if !keep.contains(&current) {
                            next_compressed.push(tree.node(current).entry.id().to_string());
                        }
                        pending.push((
                            child,
                            next_compressed,
                            if keep.contains(&current) { None } else { current_preview.clone() },
                        ));
                    }
                    let _ = child_projected;
                }
                continue;
            }
            let mut child = source_child;
            let mut compressed = Vec::new();
            let mut branch_preview = preview(&Value::Object(tree.node(child).entry.raw().clone()));
            while !keep.contains(&child) && tree.node(child).children.len() == 1 {
                compressed.push(tree.node(child).entry.id().to_string());
                child = tree.node(child).children[0];
                if branch_preview.is_none() {
                    branch_preview = preview(&Value::Object(tree.node(child).entry.raw().clone()));
                }
            }
            if !keep.contains(&child) {
                continue;
            }
            let index = projected.len();
            projected.push(ProjectedNode { source: child, children: Vec::new(), compressed, branch_preview });
            projected[parent].children.push(index);
            tasks.push((child, index, depth + 1));
        }
    }
    let mut values = vec![Value::Null; projected.len()];
    for index in (0..projected.len()).rev() {
        let children = projected[index].children.iter().map(|child| values[*child].clone()).collect();
        values[index] = node_value(tree, &projected[index], children, summary);
    }
    Value::Array(projected_roots.iter().map(|root| values[*root].clone()).collect())
}
