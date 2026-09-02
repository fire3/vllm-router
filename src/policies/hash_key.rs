//! Shared hash key extraction for consistent hashing policies
//!
//! Extracts routing keys from HTTP headers and request bodies.
//! Used by both ConsistentHashPolicy and RendezvousHashPolicy.

use super::RequestHeaders;
use crate::config::SessionAffinityConfig;
use crate::policies::ConsistentHashPolicy;
use serde_json::Value;
use tracing::debug;

/// Built-in HTTP header names to check for a session/thread/user identity
/// (case-insensitive, checked in order).
///
/// The first entries cover the session identifiers used by mainstream coding
/// agents:
/// - `x-session-id` / `session-id` / `session_id`: generic and OpenAI
///   Responses family (Codex CLI, Pi, Roo Code, Cline);
/// - `x-claude-code-session-id`: Claude Code;
/// - `x-session-affinity` / `x-opencode-session`: OpenCode/Pi;
/// - `thread-id`: Codex thread/conversation identity.
///
/// `x-user-id` / `x-tenant-id` keep the legacy user/tenant affinity behavior,
/// and the correlation/request/trace headers remain at the end for
/// backward compatibility. Operators that consider request IDs per-request can
/// override this list with a session-affinity config file.
pub(crate) const SESSION_HEADER_NAMES: &[&str] = &[
    "x-session-id",
    "x-claude-code-session-id",
    "x-session-affinity",
    "x-opencode-session",
    "session-id",
    "session_id",
    "thread-id",
    "x-user-id",
    "x-tenant-id",
    "x-correlation-id", // per-session — check before per-request
    "x-request-id",
    "x-trace-id",
];

/// Extract hash key with the built-in session-affinity settings.
///
/// Priority order:
/// 1. Session/thread/user HTTP headers (agent-aware defaults)
/// 2. Body session/user fields (incl. Anthropic metadata / Responses client_metadata)
/// 3. Hash of the first user prompt (stable for multi-turn coding agents)
/// 4. Legacy fallback: hash of the full request body (long) or raw text (short)
pub(crate) fn extract_hash_key(
    request_text: Option<&str>,
    headers: Option<&RequestHeaders>,
) -> String {
    extract_hash_key_with_config(request_text, headers, &SessionAffinityConfig::default())
}

/// Extract hash key using a `SessionAffinityConfig` (supports operator-defined
/// header names, body extraction toggles and first-user-prompt fallback).
pub(crate) fn extract_hash_key_with_config(
    request_text: Option<&str>,
    headers: Option<&RequestHeaders>,
    config: &SessionAffinityConfig,
) -> String {
    // 1. First priority: HTTP headers
    if let Some(hdrs) = headers {
        if let Some(key) = extract_hash_key_from_headers_with_config(hdrs, config) {
            return key;
        }
    }

    // 2. Second priority: body session/user fields (opt-in via config)
    if config.use_body_session_fields {
        if let Some(key) = extract_hash_key_from_body(request_text) {
            return key;
        }
    }

    // 3. Stable fallback: hash of the first user prompt, which multi-turn
    // coding agents keep stable across the conversation.
    if config.fallback_to_first_user_prompt {
        if let Some(key) = extract_first_user_prompt_hash(request_text) {
            return key;
        }
    }

    // 4. Legacy final fallback: hash of the request body
    let text = request_text.unwrap_or("");
    if text.len() > 100 {
        format!("request_hash:{:016x}", ConsistentHashPolicy::fbi_hash(text))
    } else {
        format!("request:{}", text)
    }
}

/// Extract hash key from HTTP headers
#[cfg(test)]
pub(crate) fn extract_hash_key_from_headers(headers: &RequestHeaders) -> Option<String> {
    extract_hash_key_from_headers_with_config(headers, &SessionAffinityConfig::default())
}

fn extract_hash_key_from_headers_with_config(
    headers: &RequestHeaders,
    config: &SessionAffinityConfig,
) -> Option<String> {
    for header_name in effective_header_names(config) {
        let normalized = header_name.to_ascii_lowercase();
        if let Some(raw) = headers.get(&normalized) {
            let value = raw.trim();
            if !value.is_empty() {
                debug!(
                    "Hash key extraction: found session key in header '{}': {}",
                    normalized, value
                );
                return Some(format!("header:{}:{}", normalized, value));
            }
        }
    }
    None
}

/// Resolve the ordered header list from config: full override, or built-in
/// defaults, always followed by operator-supplied extras. Names are matched
/// case-insensitively (request headers are stored lowercased by the router).
fn effective_header_names(config: &SessionAffinityConfig) -> Vec<&str> {
    let mut names: Vec<&str> = match &config.session_headers {
        Some(custom) => custom.iter().map(String::as_str).collect(),
        None => SESSION_HEADER_NAMES.to_vec(),
    };
    for extra in &config.extra_session_headers {
        names.push(extra.as_str());
    }
    names
}

/// Extract hash key from request body fields
///
/// Priority (JSON-aware):
/// session_params.session_id
///   > Anthropic metadata.session_id / metadata.user_id `_session_<uuid>`
///   > Responses client_metadata.session_id / thread_id
///   > prompt_cache_key / conversation_id / session_id / thread_id
///   > user / user_id
///
/// If the body is not valid JSON the legacy text scanner below is used.
pub(crate) fn extract_hash_key_from_body(request_text: Option<&str>) -> Option<String> {
    let text = request_text.unwrap_or("");
    if text.is_empty() {
        return None;
    }

    if let Some(key) = extract_hash_key_from_body_json(text) {
        return Some(key);
    }

    // 1. Try to extract session_params.session_id first (highest priority in body)
    if let Some(session_id) = extract_nested_field_value(text, "session_params", "session_id") {
        debug!(
            "Hash key extraction: found session_params.session_id: {}",
            session_id
        );
        return Some(format!("session:{}", session_id));
    }

    // 2. Try to extract direct user field (from OpenAI ChatCompletion/Completion requests)
    if let Some(user) = extract_field_value(text, "user") {
        debug!("Hash key extraction: found user field: {}", user);
        return Some(format!("user:{}", user));
    }

    // 3. Fallback: try legacy session_id field
    if let Some(session_id) = extract_field_value(text, "session_id") {
        return Some(format!("session:{}", session_id));
    }

    // 4. Fallback: try legacy user_id field
    if let Some(user_id) = extract_field_value(text, "user_id") {
        return Some(format!("user:{}", user_id));
    }

    None
}

/// JSON-aware body extraction. Returns `None` for non-JSON text or when no
/// known session/user field exists (avoids false matches inside prompts/tools).
fn extract_hash_key_from_body_json(text: &str) -> Option<String> {
    let root: Value = serde_json::from_str(text).ok()?;
    extract_hash_key_from_body_value(&root)
}

fn extract_hash_key_from_body_value(root: &Value) -> Option<String> {
    let obj = root.as_object()?;

    // 1. Explicit application-defined session (OpenAI extensions).
    if let Some(params) = obj.get("session_params").and_then(Value::as_object) {
        if let Some(session_id) = string_field(params, "session_id") {
            debug!(
                "Hash key extraction: found session_params.session_id: {}",
                session_id
            );
            return Some(format!("session:{}", session_id));
        }
    }

    // 2. Anthropic Messages metadata.
    if let Some(metadata) = obj.get("metadata").and_then(Value::as_object) {
        if let Some(session_id) = string_field(metadata, "session_id") {
            debug!(
                "Hash key extraction: found metadata.session_id: {}",
                session_id
            );
            return Some(format!("session:{}", session_id));
        }
        if let Some(user_id) = string_field(metadata, "user_id") {
            if let Some(session_id) = anthropic_session_suffix(user_id) {
                debug!(
                    "Hash key extraction: found metadata.user_id session suffix: {}",
                    session_id
                );
                return Some(format!("session:{}", session_id));
            }
            if !user_id.is_empty() {
                debug!(
                    "Hash key extraction: found metadata.user_id (user-level): {}",
                    user_id
                );
                return Some(format!("user:{}", user_id));
            }
        }
    }

    // 3. OpenAI Responses API client_metadata (Codex CLI / Pi).
    if let Some(client_metadata) = obj.get("client_metadata").and_then(Value::as_object) {
        if let Some(session_id) = string_field(client_metadata, "session_id") {
            debug!(
                "Hash key extraction: found client_metadata.session_id: {}",
                session_id
            );
            return Some(format!("session:{}", session_id));
        }
        if let Some(thread_id) = string_field(client_metadata, "thread_id") {
            debug!(
                "Hash key extraction: found client_metadata.thread_id: {}",
                thread_id
            );
            return Some(format!("thread:{}", thread_id));
        }
    }

    // 4. Top-level conversation/session/cache fields.
    if let Some(prompt_cache_key) = string_field(obj, "prompt_cache_key") {
        debug!(
            "Hash key extraction: found prompt_cache_key: {}",
            prompt_cache_key
        );
        return Some(format!("session:{}", prompt_cache_key));
    }
    if let Some(conversation_id) = string_field(obj, "conversation_id") {
        debug!(
            "Hash key extraction: found conversation_id: {}",
            conversation_id
        );
        return Some(format!("session:{}", conversation_id));
    }
    if let Some(session_id) = string_field(obj, "session_id") {
        debug!(
            "Hash key extraction: found top-level session_id: {}",
            session_id
        );
        return Some(format!("session:{}", session_id));
    }
    if let Some(thread_id) = string_field(obj, "thread_id") {
        debug!(
            "Hash key extraction: found top-level thread_id: {}",
            thread_id
        );
        return Some(format!("thread:{}", thread_id));
    }

    // 5. Legacy user fields (OpenAI-compatible and completion requests).
    if let Some(user) = string_field(obj, "user") {
        debug!("Hash key extraction: found user field: {}", user);
        return Some(format!("user:{}", user));
    }
    if let Some(user_id) = string_field(obj, "user_id") {
        if let Some(session_id) = anthropic_session_suffix(user_id) {
            debug!(
                "Hash key extraction: found user_id session suffix: {}",
                session_id
            );
            return Some(format!("session:{}", session_id));
        }
        debug!("Hash key extraction: found user_id field: {}", user_id);
        return Some(format!("user:{}", user_id));
    }

    None
}

fn string_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Extract the conversation/session UUID Claude Code embeds in
/// `metadata.user_id` as `user_<account>_account__session_<uuid>`.
fn anthropic_session_suffix(user_id: &str) -> Option<String> {
    const MARKER: &str = "_session_";
    let pos = user_id.rfind(MARKER)?;
    let suffix = user_id[pos + MARKER.len()..].trim();
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

/// Fallback: hash of the first user prompt extracted from Chat Completions,
/// Anthropic Messages, or OpenAI Responses request bodies.
fn extract_first_user_prompt_hash(request_text: Option<&str>) -> Option<String> {
    let text = request_text?;
    let root: Value = serde_json::from_str(text).ok()?;
    let prompt = first_user_prompt(&root)?;
    debug!(
        "Hash key extraction: first-user-prompt fallback ({} bytes)",
        prompt.len()
    );
    Some(format!(
        "first_user_prompt:{:016x}",
        ConsistentHashPolicy::fbi_hash(&prompt)
    ))
}

fn first_user_prompt(root: &Value) -> Option<String> {
    let obj = root.as_object()?;

    // Chat Completions / Anthropic Messages.
    if let Some(items) = obj.get("messages").and_then(Value::as_array) {
        if let Some(prompt) = first_user_prompt_in_items(items) {
            return Some(prompt);
        }
    }

    // OpenAI Responses API `input` item list.
    if let Some(items) = obj.get("input").and_then(Value::as_array) {
        if let Some(prompt) = first_user_prompt_in_items(items) {
            return Some(prompt);
        }
    }

    // Legacy `/v1/completions` style payloads.
    obj.get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .map(str::to_string)
}

fn first_user_prompt_in_items(items: &[Value]) -> Option<String> {
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };

        let role_user = string_field(obj, "role") == Some("user");
        let content = if role_user {
            obj.get("content")
        } else if let Some(message) = obj.get("message").and_then(Value::as_object) {
            if string_field(message, "role") == Some("user") {
                message.get("content")
            } else {
                None
            }
        } else {
            None
        };

        if let Some(content) = content {
            if let Some(prompt) = content_text(content) {
                return Some(prompt);
            }
        }
    }
    None
}

fn content_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        let text = text.trim();
        return if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        };
    }

    let blocks = content.as_array()?;
    let mut parts = Vec::new();
    for block in blocks {
        let Some(obj) = block.as_object() else {
            continue;
        };
        for key in ["text", "input_text"] {
            if let Some(text) = string_field(obj, key) {
                parts.push(text.to_string());
                break;
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Extract nested field value like session_params.session_id from JSON text
pub(crate) fn extract_nested_field_value(
    text: &str,
    parent_field: &str,
    child_field: &str,
) -> Option<String> {
    if let Some(parent_start) = find_field_start(text, parent_field) {
        if let Some(obj_start) = text[parent_start..].find('{') {
            let obj_start_pos = parent_start + obj_start;
            if let Some(obj_content) = extract_json_object(&text[obj_start_pos..]) {
                return extract_field_value(&obj_content, child_field);
            }
        }
    }
    None
}

/// Find the start position after the colon of a field in JSON text
pub(crate) fn find_field_start(text: &str, field_name: &str) -> Option<usize> {
    let patterns = [format!("\"{}\"", field_name), format!("'{}'", field_name)];

    for pattern in &patterns {
        if let Some(field_pos) = text.find(pattern) {
            let after_field = &text[field_pos + pattern.len()..];
            for (i, ch) in after_field.char_indices() {
                if ch == ':' {
                    return Some(field_pos + pattern.len() + i + 1);
                } else if !ch.is_whitespace() {
                    break;
                }
            }
        }
    }
    None
}

/// Extract JSON object content (simple brace matching)
pub(crate) fn extract_json_object(text: &str) -> Option<String> {
    if !text.starts_with('{') {
        return None;
    }

    let mut brace_count = 0;
    let mut end_pos = 0;

    for (i, ch) in text.char_indices() {
        match ch {
            '{' => brace_count += 1,
            '}' => {
                brace_count -= 1;
                if brace_count == 0 {
                    end_pos = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }

    if brace_count == 0 && end_pos > 0 {
        Some(text[0..end_pos].to_string())
    } else {
        None
    }
}

/// Extract field value from JSON-like text (simple parser)
///
/// Supports double-quoted, single-quoted, and unquoted values.
pub(crate) fn extract_field_value(text: &str, field_name: &str) -> Option<String> {
    let patterns = [
        format!("\"{}\"", field_name),
        format!("'{}'", field_name),
        field_name.to_string(),
    ];

    for pattern in &patterns {
        if let Some(field_pos) = text.find(pattern) {
            let after_field = &text[field_pos + pattern.len()..];

            // Skip whitespace and look for colon
            let mut colon_pos = None;
            for (i, ch) in after_field.char_indices() {
                if ch == ':' {
                    colon_pos = Some(i);
                    break;
                } else if !ch.is_whitespace() {
                    break;
                }
            }

            if let Some(colon_idx) = colon_pos {
                let after_colon = &after_field[colon_idx + 1..];
                let trimmed = after_colon.trim_start();

                // Extract quoted string (double or single quotes)
                if trimmed.starts_with('"') {
                    if let Some(stripped) = trimmed.strip_prefix('"') {
                        if let Some(end_quote) = stripped.find('"') {
                            return Some(stripped[..end_quote].to_string());
                        }
                    }
                } else if trimmed.starts_with('\'') {
                    if let Some(stripped) = trimmed.strip_prefix('\'') {
                        if let Some(end_quote) = stripped.find('\'') {
                            return Some(stripped[..end_quote].to_string());
                        }
                    }
                } else {
                    // Unquoted value - extract until delimiter
                    let end_pos = trimmed
                        .find(&[',', ' ', '}', ']', '\n', '\r', '\t'][..])
                        .unwrap_or(trimmed.len());
                    if end_pos > 0 {
                        return Some(trimmed[..end_pos].to_string());
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // === extract_field_value tests ===

    #[test]
    fn test_extract_field_value_double_quoted() {
        let text = r#"{"session_id": "abc123", "prompt": "hello"}"#;
        assert_eq!(
            extract_field_value(text, "session_id"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_extract_field_value_single_quoted() {
        let text = r#"{'session_id': 'def456', 'prompt': 'world'}"#;
        assert_eq!(
            extract_field_value(text, "session_id"),
            Some("def456".to_string())
        );
    }

    #[test]
    fn test_extract_field_value_unquoted() {
        let text = r#"{"count": 42, "name": "test"}"#;
        assert_eq!(extract_field_value(text, "count"), Some("42".to_string()));
    }

    #[test]
    fn test_extract_field_value_missing() {
        let text = r#"{"other": "val"}"#;
        assert_eq!(extract_field_value(text, "session_id"), None);
    }

    #[test]
    fn test_extract_field_value_no_space_after_colon() {
        let text = r#"{"session_id":"compact_value"}"#;
        assert_eq!(
            extract_field_value(text, "session_id"),
            Some("compact_value".to_string())
        );
    }

    #[test]
    fn test_extract_field_value_multiple_fields() {
        let text = r#"{"user": "bob", "prompt": "hi", "session_id": "sess1"}"#;
        assert_eq!(extract_field_value(text, "user"), Some("bob".to_string()));
        assert_eq!(
            extract_field_value(text, "session_id"),
            Some("sess1".to_string())
        );
    }

    // === extract_nested_field_value tests ===

    #[test]
    fn test_extract_nested_field_value() {
        let text = r#"{"session_params": {"session_id": "nested123"}, "prompt": "hi"}"#;
        assert_eq!(
            extract_nested_field_value(text, "session_params", "session_id"),
            Some("nested123".to_string())
        );
    }

    #[test]
    fn test_extract_nested_field_value_missing_parent() {
        let text = r#"{"prompt": "hi"}"#;
        assert_eq!(
            extract_nested_field_value(text, "session_params", "session_id"),
            None
        );
    }

    #[test]
    fn test_extract_nested_field_value_missing_child() {
        let text = r#"{"session_params": {"other": "val"}, "prompt": "hi"}"#;
        assert_eq!(
            extract_nested_field_value(text, "session_params", "session_id"),
            None
        );
    }

    // === extract_json_object tests ===

    #[test]
    fn test_extract_json_object_simple() {
        let text = r#"{"key": "value"} trailing"#;
        assert_eq!(
            extract_json_object(text),
            Some(r#"{"key": "value"}"#.to_string())
        );
    }

    #[test]
    fn test_extract_json_object_nested() {
        let text = r#"{"outer": {"inner": "val"}} trailing"#;
        assert_eq!(
            extract_json_object(text),
            Some(r#"{"outer": {"inner": "val"}}"#.to_string())
        );
    }

    #[test]
    fn test_extract_json_object_not_object() {
        assert_eq!(extract_json_object("not json"), None);
    }

    #[test]
    fn test_extract_json_object_unclosed() {
        assert_eq!(extract_json_object("{unclosed"), None);
    }

    // === extract_hash_key_from_headers tests ===

    #[test]
    fn test_header_extraction_priority() {
        let mut headers = HashMap::new();
        headers.insert("x-request-id".to_string(), "req-1".to_string());
        headers.insert("x-session-id".to_string(), "sess-1".to_string());

        // x-session-id has higher priority than x-request-id
        let key = extract_hash_key_from_headers(&headers).unwrap();
        assert_eq!(key, "header:x-session-id:sess-1");
    }

    #[test]
    fn test_header_extraction_skips_empty() {
        let mut headers = HashMap::new();
        headers.insert("x-session-id".to_string(), "".to_string());
        headers.insert("x-user-id".to_string(), "user-1".to_string());

        let key = extract_hash_key_from_headers(&headers).unwrap();
        assert_eq!(key, "header:x-user-id:user-1");
    }

    #[test]
    fn test_header_extraction_no_match() {
        let mut headers = HashMap::new();
        headers.insert("x-custom-header".to_string(), "val".to_string());
        assert_eq!(extract_hash_key_from_headers(&headers), None);
    }

    // === extract_hash_key_from_body tests ===

    #[test]
    fn test_body_extraction_session_params_priority() {
        // session_params.session_id takes priority over direct session_id
        let text = r#"{"session_params": {"session_id": "nested"}, "session_id": "direct"}"#;
        let key = extract_hash_key_from_body(Some(text)).unwrap();
        assert_eq!(key, "session:nested");
    }

    #[test]
    fn test_body_extraction_user_field() {
        let text = r#"{"user": "alice", "prompt": "hi"}"#;
        let key = extract_hash_key_from_body(Some(text)).unwrap();
        assert_eq!(key, "user:alice");
    }

    #[test]
    fn test_body_extraction_legacy_session_id() {
        let text = r#"{"session_id": "legacy123", "prompt": "hi"}"#;
        let key = extract_hash_key_from_body(Some(text)).unwrap();
        assert_eq!(key, "session:legacy123");
    }

    #[test]
    fn test_body_extraction_legacy_user_id() {
        let text = r#"{"user_id": "uid456", "prompt": "hi"}"#;
        let key = extract_hash_key_from_body(Some(text)).unwrap();
        assert_eq!(key, "user:uid456");
    }

    #[test]
    fn test_body_extraction_empty() {
        assert_eq!(extract_hash_key_from_body(None), None);
        assert_eq!(extract_hash_key_from_body(Some("")), None);
    }

    #[test]
    fn test_body_extraction_no_known_fields() {
        let text = r#"{"prompt": "hello", "model": "llama"}"#;
        assert_eq!(extract_hash_key_from_body(Some(text)), None);
    }

    // === extract_hash_key (top-level) tests ===

    #[test]
    fn test_hash_key_headers_over_body() {
        let mut headers = HashMap::new();
        headers.insert("x-session-id".to_string(), "from-header".to_string());
        let body = r#"{"session_id": "from-body"}"#;

        let key = extract_hash_key(Some(body), Some(&headers));
        assert_eq!(key, "header:x-session-id:from-header");
    }

    #[test]
    fn test_hash_key_fallback_short_text() {
        let key = extract_hash_key(Some("short"), None);
        assert_eq!(key, "request:short");
    }

    #[test]
    fn test_hash_key_fallback_long_text() {
        let long_text = "x".repeat(200);
        let key = extract_hash_key(Some(&long_text), None);
        assert!(key.starts_with("request_hash:"));
        assert_eq!(key.len(), "request_hash:".len() + 16); // 16 hex chars

        // Same long text should produce same hash
        let key2 = extract_hash_key(Some(&long_text), None);
        assert_eq!(key, key2);
    }

    #[test]
    fn test_hash_key_fallback_none() {
        let key = extract_hash_key(None, None);
        assert_eq!(key, "request:");
    }

    // === find_field_start tests ===

    #[test]
    fn test_find_field_start_double_quoted() {
        let text = r#"{"field": "value"}"#;
        let pos = find_field_start(text, "field");
        assert!(pos.is_some());
        // Should point to after the colon
        let after = &text[pos.unwrap()..];
        assert!(after.trim_start().starts_with('"'));
    }

    #[test]
    fn test_find_field_start_single_quoted() {
        let text = r#"{'field': 'value'}"#;
        let pos = find_field_start(text, "field");
        assert!(pos.is_some());
    }

    #[test]
    fn test_find_field_start_missing() {
        let text = r#"{"other": "value"}"#;
        assert_eq!(find_field_start(text, "field"), None);
    }

    // === Agent-native session identifiers ===

    #[test]
    fn test_agent_session_headers_are_supported() {
        let mut headers = HashMap::new();
        headers.insert("x-request-id".to_string(), "req-1".to_string());
        headers.insert(
            "x-claude-code-session-id".to_string(),
            "claude-session-1".to_string(),
        );
        let key = extract_hash_key_from_headers(&headers).unwrap();
        assert_eq!(key, "header:x-claude-code-session-id:claude-session-1");
    }

    #[test]
    fn test_codex_session_and_thread_headers_are_supported() {
        let mut headers = HashMap::new();
        headers.insert("session-id".to_string(), "ses_abc".to_string());
        headers.insert("thread-id".to_string(), "thread_1".to_string());
        let key = extract_hash_key_from_headers(&headers).unwrap();
        assert_eq!(key, "header:session-id:ses_abc");

        let mut thread_only = HashMap::new();
        thread_only.insert("thread-id".to_string(), "thread_1".to_string());
        let key = extract_hash_key_from_headers(&thread_only).unwrap();
        assert_eq!(key, "header:thread-id:thread_1");
    }

    #[test]
    fn test_custom_header_config_overrides_builtins() {
        let mut headers = HashMap::new();
        headers.insert("x-session-id".to_string(), "generic".to_string());
        headers.insert("x-my-session".to_string(), "custom".to_string());
        let config = SessionAffinityConfig {
            session_headers: Some(vec!["x-my-session".to_string()]),
            ..SessionAffinityConfig::default()
        };
        let key = extract_hash_key_from_headers_with_config(&headers, &config).unwrap();
        assert_eq!(key, "header:x-my-session:custom");
    }

    // === Body fields used by coding agents ===

    #[test]
    fn test_body_extraction_anthropic_metadata_session_suffix() {
        let text = r#"{
            "model": "claude-sonnet",
            "metadata": {
                "user_id": "user_abc_account__session_11111111-2222-3333-4444-555555555555"
            }
        }"#;
        let key = extract_hash_key_from_body(Some(text)).unwrap();
        assert_eq!(key, "session:11111111-2222-3333-4444-555555555555");
    }

    #[test]
    fn test_body_extraction_responses_client_metadata() {
        let session = r#"{
            "model": "gpt-5-codex",
            "client_metadata": {"session_id": "ses_abc", "thread_id": "thread_1"}
        }"#;
        let key = extract_hash_key_from_body(Some(session)).unwrap();
        assert_eq!(key, "session:ses_abc");

        let thread_only = r#"{
            "client_metadata": {"thread_id": "thread_1"}
        }"#;
        let key = extract_hash_key_from_body(Some(thread_only)).unwrap();
        assert_eq!(key, "thread:thread_1");
    }

    #[test]
    fn test_body_extraction_conversation_id_and_prompt_cache_key() {
        assert_eq!(
            extract_hash_key_from_body(Some(r#"{"conversation_id": "conv-1"}"#)),
            Some("session:conv-1".to_string())
        );
        assert_eq!(
            extract_hash_key_from_body(Some(r#"{"prompt_cache_key": "ses_abc"}"#)),
            Some("session:ses_abc".to_string())
        );
    }

    // === First-user-prompt fallback ===

    #[test]
    fn test_first_user_prompt_fallback_is_stable_across_turns() {
        let first_turn = r#"{
            "model": "llama-3",
            "messages": [
                {"role": "system", "content": "be helpful"},
                {"role": "user", "content": "hello world"}
            ]
        }"#;
        let later_turn = r#"{
            "model": "llama-3",
            "messages": [
                {"role": "system", "content": "be helpful"},
                {"role": "user", "content": "hello world"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "one more question"}
            ]
        }"#;

        let key1 = extract_hash_key(Some(first_turn), None);
        let key2 = extract_hash_key(Some(later_turn), None);
        assert!(key1.starts_with("first_user_prompt:"), "got {}", key1);
        assert_eq!(key1, key2);

        let different_first_prompt = later_turn.replace("hello world", "different start");
        let key3 = extract_hash_key(Some(&different_first_prompt), None);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_first_user_prompt_fallback_supports_anthropic_and_responses_shapes() {
        let anthropic = r#"{
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "claude first prompt"}]
            }]
        }"#;
        let responses = r#"{
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "codex first prompt"}]
            }]
        }"#;
        assert!(extract_hash_key(Some(anthropic), None).starts_with("first_user_prompt:"));
        assert!(extract_hash_key(Some(responses), None).starts_with("first_user_prompt:"));
    }

    #[test]
    fn test_first_user_prompt_fallback_can_be_disabled() {
        let body = r#"{
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        }"#;
        let config = SessionAffinityConfig {
            fallback_to_first_user_prompt: false,
            ..SessionAffinityConfig::default()
        };
        let key = extract_hash_key_with_config(Some(body), None, &config);
        assert!(key.starts_with("request_hash:"), "got {}", key);
    }
}
