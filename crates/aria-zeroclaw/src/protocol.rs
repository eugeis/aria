//! Wire protocol for ZeroClaw's `GET /ws/chat` gateway endpoint.
//!
//! Client -> server frames:
//!   {"type":"message","content":"..."}
//!   {"type":"approval_response","request_id":"...","decision":"approve|deny|always"}
//!
//! Server -> client frames (subset we consume):
//!   {"type":"chunk","content":"<delta>"}
//!   {"type":"done","full_response":"...","cost_usd":...}
//!   {"type":"approval_request","request_id":"...","tool":"...","arguments_summary":"...","timeout_secs":n}

use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum InFrame {
    #[serde(rename = "chunk")]
    Chunk { content: String },
    #[serde(rename = "done")]
    Done {
        #[serde(default)]
        full_response: String,
        #[serde(default)]
        cost_usd: Option<f64>,
    },
    #[serde(rename = "approval_request")]
    ApprovalRequest {
        request_id: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        arguments_summary: String,
        #[serde(default)]
        timeout_secs: u64,
    },
    #[serde(other)]
    Other,
}

pub fn message_frame(text: &str) -> String {
    json!({ "type": "message", "content": text }).to_string()
}

pub fn approval_frame(request_id: &str, decision: &str) -> String {
    json!({ "type": "approval_response", "request_id": request_id, "decision": decision })
        .to_string()
}

/// `agent_alias`, `session_id`, `token` query-parameter builder for the
/// chat WebSocket.
///
/// Built manually because the `url` crate rejects `ws://`/`wss://` schemes.
pub fn chat_url(
    base: &str,
    agent_alias: &str,
    session_id: &str,
    token: Option<&str>,
) -> Option<String> {
    use url::form_urlencoded;
    let mut base = base.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return None;
    }
    if !base.starts_with("ws://") && !base.starts_with("wss://") {
        base = format!("ws://{base}");
    }
    // Accept a base that already ends in /ws/chat.
    if !base.ends_with("/ws/chat") {
        base = format!("{base}/ws/chat");
    }
    let mut q = vec![
        ("agent_alias".to_string(), agent_alias.to_string()),
        ("session_id".to_string(), session_id.to_string()),
    ];
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        q.push(("token".to_string(), t.to_string()));
    }
    let query = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(q)
        .finish();
    Some(format!("{base}?{query}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_chat_url() {
        let u = chat_url("ws://127.0.0.1:3000", "voice", "alexa-dev1", Some("sekret")).unwrap();
        assert!(u.starts_with("ws://127.0.0.1:3000/ws/chat?"));
        assert!(u.contains("agent_alias=voice"));
        assert!(u.contains("session_id=alexa-dev1"));
        assert!(u.contains("token=sekret"));
    }

    #[test]
    fn parses_chunk_done_approval() {
        let c: InFrame = serde_json::from_str(r#"{"type":"chunk","content":"hi"}"#).unwrap();
        assert!(matches!(c, InFrame::Chunk { content } if content == "hi"));
        let d: InFrame =
            serde_json::from_str(r#"{"type":"done","full_response":"hello","cost_usd":0.01}"#)
                .unwrap();
        match d {
            InFrame::Done {
                full_response,
                cost_usd,
            } => {
                assert_eq!(full_response, "hello");
                assert_eq!(cost_usd, Some(0.01));
            }
            _ => panic!(),
        }
        let a: InFrame = serde_json::from_str(
            r#"{"type":"approval_request","request_id":"r1","tool":"shell","arguments_summary":"rm -rf /tmp/x","timeout_secs":120}"#,
        )
        .unwrap();
        match a {
            InFrame::ApprovalRequest {
                request_id,
                tool,
                timeout_secs,
                ..
            } => {
                assert_eq!(request_id, "r1");
                assert_eq!(tool, "shell");
                assert_eq!(timeout_secs, 120);
            }
            _ => panic!(),
        }
        let o: InFrame = serde_json::from_str(r#"{"type":"history_trimmed"}"#).unwrap();
        assert!(matches!(o, InFrame::Other));
    }
}
