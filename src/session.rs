//! Session identity.
//!
//! GitHub Copilot CLI (and most OpenAI-compatible clients) send no session ID.
//! They resend the full message history on every request, so we fingerprint the
//! conversation: the first user message is stable across all turns of a session
//! and distinct between sessions.
//!
//! Precedence:
//!   1. `X-Session-ID` header (explicit client-provided session)
//!   2. `session_id` / `thread_id` field in the request body
//!   3. hash(model + NUL + first user message content)

use http::HeaderMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Maximum length of a conversation ID forwarded upstream before it is hashed
/// (OpenRouter's 256-char limit).
const MAX_UPSTREAM_VALUE_CHARS: usize = 256;

/// Characters of the fingerprint hash kept for the internal session key.
const FINGERPRINT_PREFIX_LEN: usize = 24;

/// Derive a session key from an incoming request so the load balancer can route
/// consecutive turns of the same conversation to the same backend.
pub fn session_key_from_body(headers: &HeaderMap, body: &Value) -> Option<String> {
    if let Some(header) = headers.get("x-session-id").and_then(|v| v.to_str().ok()) {
        if !header.is_empty() {
            return Some(format!("h:{header}"));
        }
    }

    if let Value::Object(obj) = body {
        for field in ["session_id", "thread_id"] {
            if let Some(Value::String(id)) = obj.get(field) {
                if !id.is_empty() {
                    return Some(format!("b:{id}"));
                }
            }
        }
        if let Some(Value::Array(messages)) = obj.get("messages") {
            let first_user = messages.iter().find(|m| {
                matches!(m, Value::Object(m) if m.get("role").and_then(Value::as_str) == Some("user"))
            });
            if let Some(Value::Object(first_user)) = first_user {
                let content = match first_user.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => serde_json::to_string(other).unwrap_or_default(),
                };
                let model = match obj.get("model") {
                    Some(Value::String(s)) => s.as_str(),
                    _ => "",
                };
                let hash = sha256_hex(&format!("{model}\u{0}{content}"));
                return Some(format!("f:{}", &hash[..FINGERPRINT_PREFIX_LEN]));
            }
        }
    }

    None
}

/// Normalize an internal session key (`h:…` / `b:…` / `f:…`) into a value safe
/// to send upstream as a conversation-ID header.
///
/// Strips the internal single-char prefix, trims whitespace, passes values
/// through unchanged when they fit the 256-char limit, and falls back to a
/// deterministic SHA-256 hex digest when overlong. Returns None when there is
/// no usable value (no session key).
pub fn upstream_session_value(session_key: Option<&str>) -> Option<String> {
    let session_key = session_key?;
    let value = match session_key.as_bytes() {
        [b'h' | b'b' | b'f', b':', rest @ ..] => std::str::from_utf8(rest).ok()?,
        _ => session_key,
    };
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.chars().count() <= MAX_UPSTREAM_VALUE_CHARS {
        Some(value.to_string())
    } else {
        Some(sha256_hex(value))
    }
}

/// Lowercase hex SHA-256 digest of `input`.
pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("nibble"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let name = HeaderName::from_bytes(name.as_bytes()).expect("header name");
            map.insert(name, HeaderValue::from_str(value).expect("header value"));
        }
        map
    }

    fn empty_headers() -> HeaderMap {
        headers(&[])
    }

    #[test]
    fn same_conversation_produces_the_same_key_across_turns() {
        let turn1 = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "you are a bot"},
                    {"role": "user", "content": "hello"}
                ]
            }),
        );
        let turn2 = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "you are a bot"},
                    {"role": "user", "content": "hello"},
                    {"role": "assistant", "content": "hi"},
                    {"role": "user", "content": "say more"}
                ]
            }),
        );
        assert!(turn1.is_some() && turn2.is_some());
        assert_eq!(turn1, turn2);
        assert!(turn1.unwrap().starts_with("f:"));
    }

    #[test]
    fn different_first_user_message_produces_a_different_key() {
        let k1 = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"messages": [{"role": "user", "content": "aaa"}]}),
        );
        let k2 = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"messages": [{"role": "user", "content": "bbb"}]}),
        );
        assert_ne!(k1, k2);
    }

    #[test]
    fn model_is_part_of_the_fingerprint() {
        let k1 = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"model": "a", "messages": [{"role": "user", "content": "hi"}]}),
        );
        let k2 = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"model": "b", "messages": [{"role": "user", "content": "hi"}]}),
        );
        assert_ne!(k1, k2);
    }

    #[test]
    fn missing_model_is_treated_as_empty_string() {
        let without = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
        );
        let empty = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"model": "", "messages": [{"role": "user", "content": "hi"}]}),
        );
        assert_eq!(without, empty);
    }

    #[test]
    fn non_string_user_content_is_serialized() {
        let text = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
        );
        let array = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"messages": [{"role": "user", "content": ["hi"]}]}),
        );
        assert_ne!(text, array);
    }

    #[test]
    fn system_messages_are_skipped_when_fingerprinting() {
        let key = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"messages": [
                {"role": "system", "content": "s"},
                {"role": "assistant", "content": "a"},
                {"role": "user", "content": "u"}
            ]}),
        );
        let only_user = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"messages": [{"role": "user", "content": "u"}]}),
        );
        assert_eq!(key, only_user);
    }

    #[test]
    fn x_session_id_header_wins_over_fingerprint() {
        let key = session_key_from_body(
            &headers(&[("x-session-id", "my-session")]),
            &serde_json::json!({"messages": [{"role": "user", "content": "hello"}]}),
        );
        assert_eq!(key.as_deref(), Some("h:my-session"));
    }

    #[test]
    fn session_id_in_body_is_used() {
        let key = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"session_id": "abc", "messages": []}),
        );
        assert_eq!(key.as_deref(), Some("b:abc"));
    }

    #[test]
    fn thread_id_in_body_is_used_as_a_fallback() {
        let key = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"thread_id": "t-1", "messages": []}),
        );
        assert_eq!(key.as_deref(), Some("b:t-1"));
    }

    #[test]
    fn session_id_wins_over_thread_id() {
        let key = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"session_id": "s", "thread_id": "t", "messages": []}),
        );
        assert_eq!(key.as_deref(), Some("b:s"));
    }

    #[test]
    fn empty_session_id_falls_through_to_the_fingerprint() {
        let key = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"session_id": "", "messages": [{"role": "user", "content": "hi"}]}),
        );
        assert!(key.unwrap().starts_with("f:"));
    }

    #[test]
    fn empty_body_produces_no_session_key() {
        assert_eq!(
            session_key_from_body(&empty_headers(), &serde_json::json!({"messages": []})),
            None
        );
        assert_eq!(
            session_key_from_body(&empty_headers(), &serde_json::json!({})),
            None
        );
        assert_eq!(
            session_key_from_body(&empty_headers(), &serde_json::json!([])),
            None
        );
        assert_eq!(
            session_key_from_body(&empty_headers(), &serde_json::json!(null)),
            None
        );
        assert_eq!(
            session_key_from_body(&empty_headers(), &serde_json::json!("hello")),
            None
        );
    }

    #[test]
    fn empty_x_session_id_header_is_ignored() {
        let key = session_key_from_body(
            &headers(&[("x-session-id", "")]),
            &serde_json::json!({"session_id": "abc", "messages": []}),
        );
        assert_eq!(key.as_deref(), Some("b:abc"));
    }

    #[test]
    fn upstream_session_value_strips_internal_prefixes() {
        assert_eq!(
            upstream_session_value(Some("h:my-session")).as_deref(),
            Some("my-session")
        );
        assert_eq!(
            upstream_session_value(Some("b:abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            upstream_session_value(Some("f:abc123")).as_deref(),
            Some("abc123")
        );
        // Raw values that merely contain a colon are left alone.
        assert_eq!(
            upstream_session_value(Some("ab:cd")).as_deref(),
            Some("ab:cd")
        );
        assert_eq!(
            upstream_session_value(Some("x:yz")).as_deref(),
            Some("x:yz")
        );
    }

    #[test]
    fn upstream_session_value_returns_none_without_a_usable_key() {
        assert_eq!(upstream_session_value(None), None);
        assert_eq!(upstream_session_value(Some("h:  ")), None);
        assert_eq!(upstream_session_value(Some("h:")), None);
    }

    #[test]
    fn upstream_session_value_hashes_overlong_values_deterministically() {
        let long = "x".repeat(300);
        let v1 = upstream_session_value(Some(&format!("h:{long}"))).expect("value");
        let v2 = upstream_session_value(Some(&format!("h:{long}"))).expect("value");
        assert_eq!(v1, v2);
        assert_eq!(v1.len(), 64);
        assert!(v1
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

        // Boundary: exactly 256 chars passes through unhashed.
        let boundary = "y".repeat(256);
        let value = upstream_session_value(Some(&format!("b:{boundary}"))).expect("value");
        assert_eq!(value.chars().count(), 256);
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn fingerprint_uses_a_nul_separated_model_and_content() {
        let key = session_key_from_body(
            &empty_headers(),
            &serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]}),
        )
        .expect("key");
        let expected = format!("f:{}", &sha256_hex("m\u{0}hello")[..24]);
        assert_eq!(key, expected);
    }
}
