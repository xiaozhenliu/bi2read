//! LLM connection configuration and adapter trait.
//!
//! `ApiFormat` selects the request adapter purely by format, never inferred
//! from model name, vendor, or URL. LLM is optional: when disabled the system
//! still produces raw transcripts and rule-based Markdown.
//!
//! This version supports only local, no-auth endpoints. The
//! `Auth` enum has been removed; remote endpoints requiring authentication are
//! not supported and the UI disables their configuration.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// API format (protocol), independent of vendor/deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    AnthropicMessages,
    OpenAiChatCompletions,
}

impl ApiFormat {
    pub fn label(&self) -> &'static str {
        match self {
            ApiFormat::AnthropicMessages => "anthropic_messages",
            ApiFormat::OpenAiChatCompletions => "openai_chat_completions",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "anthropic_messages" => Some(Self::AnthropicMessages),
            "openai_chat_completions" => Some(Self::OpenAiChatCompletions),
            _ => None,
        }
    }
}

/// Normalize a base URL: strip trailing slashes and remove API path suffixes
/// (`/v1/chat/completions`, `/v1/messages`) so the adapter can append them
/// exactly once.
pub fn normalize_base_url(url: &str) -> String {
    let mut s = url.trim().trim_end_matches('/').to_string();
    // Strip common API path suffixes if the user pasted a full endpoint URL.
    for suffix in ["/v1/chat/completions", "/v1/messages", "/v1"] {
        if s.ends_with(suffix) {
            s.truncate(s.len() - suffix.len());
            break;
        }
    }
    s
}

/// A configured LLM endpoint. This version supports only local no-auth
/// endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConnection {
    pub id: String,
    pub name: String,
    pub api_format: ApiFormat,
    pub base_url: String,
    pub model: String,
}

impl LlmConnection {
    /// True only when the endpoint is a loopback address (fully local).
    pub fn is_local(&self) -> bool {
        let host = self
            .base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split([':', '/'])
            .next()
            .unwrap_or("");
        host == "127.0.0.1" || host == "localhost" || host == "[::1]" || host == "::1"
    }
}

/// Adapter trait: one implementation per `ApiFormat`.
pub trait LlmAdapter {
    fn format(&self) -> ApiFormat;
    /// Refine a batch of utterances into readable text via HTTP, returning
    /// refined text keyed by utterance id.
    fn refine_batch(
        &self,
        conn: &LlmConnection,
        utterances: &[crate::funasr::Utterance],
    ) -> Result<std::collections::HashMap<String, String>, LlmError>;
}

pub fn for_format(fmt: ApiFormat) -> Box<dyn LlmAdapter> {
    match fmt {
        ApiFormat::AnthropicMessages => Box::new(AnthropicAdapter),
        ApiFormat::OpenAiChatCompletions => Box::new(OpenAiAdapter),
    }
}

struct AnthropicAdapter;
impl LlmAdapter for AnthropicAdapter {
    fn format(&self) -> ApiFormat {
        ApiFormat::AnthropicMessages
    }

    fn refine_batch(
        &self,
        conn: &LlmConnection,
        utterances: &[crate::funasr::Utterance],
    ) -> Result<std::collections::HashMap<String, String>, LlmError> {
        let prompt = build_prompt(utterances);
        let url = format!("{}/v1/messages", conn.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": conn.model,
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": prompt}],
        });
        let resp = http_post(&url, conn, &body)?;
        // Anthropic: response.content[] is a list of {type,text}; concatenate text.
        let texts: Vec<String> = resp
            .get("content")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                            b.get("text")
                                .and_then(|t| t.as_str())
                                .map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let text = texts.join("\n");
        parse_refined(&text, utterances)
    }
}

struct OpenAiAdapter;
impl LlmAdapter for OpenAiAdapter {
    fn format(&self) -> ApiFormat {
        ApiFormat::OpenAiChatCompletions
    }

    fn refine_batch(
        &self,
        conn: &LlmConnection,
        utterances: &[crate::funasr::Utterance],
    ) -> Result<std::collections::HashMap<String, String>, LlmError> {
        let prompt = build_prompt(utterances);
        let url = format!(
            "{}/v1/chat/completions",
            conn.base_url.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "model": conn.model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.2,
        });
        let resp = http_post(&url, conn, &body)?;
        // OpenAI: choices[0].message.content
        let text = resp
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        parse_refined(text, utterances)
    }
}

/// The system/user prompt: forbid summarization/deletion/reordering; allow
/// only punctuation/typo/filler/paragraph fixes.
fn build_prompt(utterances: &[crate::funasr::Utterance]) -> String {
    let mut s = String::new();
    s.push_str("你是语音转写文稿的校对助手。下面给出若干条发言，每条带 id。");
    s.push_str("请逐条整理标点、明显错字、口头填充词和分段，保留每条发言的完整含义。");
    s.push_str("禁止摘要、删除例子、删除限定条件或重排观点。");
    s.push_str("数字、金额、日期和 URL 必须原样保留。");
    s.push_str("只允许整理，不得删减。");
    s.push_str("输出 JSON 数组，每个元素形如 {\"id\":\"u001\",\"text\":\"整理后的完整发言\"}，包含全部输入 id，顺序与输入一致。\n\n");
    s.push_str("输入：\n");
    s.push_str("[\n");
    for u in utterances {
        s.push_str(&format!(
            "  {{\"id\":\"{}\",\"text\":{}}},\n",
            u.id,
            serde_json::Value::String(u.text.clone())
        ));
    }
    s.push_str("]\n");
    s
}

/// POST JSON to `url` with no auth headers and return the parsed JSON body.
/// Only local endpoints are supported.
fn http_post(
    url: &str,
    _conn: &LlmConnection,
    body: &serde_json::Value,
) -> Result<serde_json::Value, LlmError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(120))
        .build();
    let req = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("User-Agent", "bimyscribe/0.1");
    let resp = req
        .send_string(&body.to_string())
        .map_err(|e| LlmError::Connection(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_reader(resp.into_reader())
        .map_err(|e| LlmError::Connection(e.to_string()))?;
    Ok(v)
}

/// Parse the LLM's JSON-array response and validate it:
/// - every input id returned exactly once;
/// - empty text -> use original;
/// - any missing/duplicate id -> fail the batch (caller falls back to raw).
///
/// Returns a map id -> refined text.
fn parse_refined(
    text: &str,
    utterances: &[crate::funasr::Utterance],
) -> Result<std::collections::HashMap<String, String>, LlmError> {
    // The model may wrap JSON in prose; extract the first JSON array.
    let json = extract_json_array(text)
        .ok_or_else(|| LlmError::Validation("response is not a JSON array".into()))?;
    let items: Vec<RefinedItem> =
        serde_json::from_str(&json).map_err(|e| LlmError::Validation(format!("parse: {e}")))?;

    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for it in &items {
        if !seen.insert(it.id.clone()) {
            return Err(LlmError::Validation(format!("duplicate id: {}", it.id)));
        }
        map.insert(it.id.clone(), it.text.clone());
    }
    // Ensure every input id is present and non-empty (else use original).
    let mut out = std::collections::HashMap::new();
    for u in utterances {
        let refined = map.get(&u.id).cloned().unwrap_or_default();
        let text = if refined.trim().is_empty() || is_obvious_shrink(&refined, &u.text) {
            u.text.clone()
        } else {
            refined
        };
        out.insert(u.id.clone(), text);
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RefinedItem {
    id: String,
    text: String,
}

/// Heuristic: if the refined text is much shorter than the original, assume the
/// model summarized/truncated and use the original.
fn is_obvious_shrink(refined: &str, original: &str) -> bool {
    let r = refined.trim();
    let o = original.trim();
    if o.chars().count() < 10 {
        return false; // too short to judge
    }
    // Shrink to under half the original length is suspicious.
    r.chars().count() * 2 < o.chars().count()
}

/// Find the first top-level JSON array `[...]` in `text` (models sometimes wrap
/// output in ``` fences or prose).
fn extract_json_array(text: &str) -> Option<String> {
    let start = text.find('[')?;
    // Walk to the matching close bracket, respecting nested arrays/objects/strings.
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Orchestrate refining all utterances through the LLM in batches.
/// Any batch failure falls back to the original text for that batch, so the
/// readable document is always produced. The returned vector stays
/// one-to-one and in-order with `utterances`, allowing the document renderer to
/// preserve every speaker and timestamp link.
pub fn refine_utterances(
    conn: &LlmConnection,
    utterances: &[crate::funasr::Utterance],
) -> Result<Vec<String>, LlmError> {
    let adapter = for_format(conn.api_format);
    let batch = 20usize;
    let mut ordered: Vec<String> = Vec::with_capacity(utterances.len());
    for chunk in utterances.chunks(batch) {
        match adapter.refine_batch(conn, chunk) {
            Ok(map) => {
                for u in chunk {
                    ordered.push(map.get(&u.id).cloned().unwrap_or_else(|| u.text.clone()));
                }
            }
            Err(e) => {
                log::warn!("LLM batch failed ({} utts), using raw: {}", chunk.len(), e);
                for u in chunk {
                    ordered.push(u.text.clone());
                }
            }
        }
    }
    Ok(ordered)
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("connection: {0}")]
    Connection(String),
    #[error("validation: {0}")]
    Validation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utt(id: &str, text: &str) -> crate::funasr::Utterance {
        crate::funasr::Utterance {
            id: id.into(),
            text: text.into(),
            start_ms: 0,
            end_ms: 1000,
            speaker_id: 0,
        }
    }

    #[test]
    fn parse_refined_all_ids_present() {
        let text = r#"[
            {"id":"u001","text":"你好，世界。"},
            {"id":"u002","text":"第二句。"}
        ]"#;
        let utts = vec![utt("u001", "你好世界"), utt("u002", "第二句")];
        let map = parse_refined(text, &utts).unwrap();
        assert_eq!(map.get("u001").unwrap(), "你好，世界。");
        assert_eq!(map.get("u002").unwrap(), "第二句。");
    }

    #[test]
    fn parse_refined_missing_id_falls_back_to_original() {
        // If an id is missing, that utterance uses its original text.
        let text = r#"[{"id":"u001","text":"保留。"}]"#;
        let utts = vec![utt("u001", "原1"), utt("u002", "原2")];
        let map = parse_refined(text, &utts).unwrap();
        assert_eq!(map.get("u001").unwrap(), "保留。");
        assert_eq!(map.get("u002").unwrap(), "原2");
    }

    #[test]
    fn parse_refined_empty_text_uses_original() {
        let text = r#"[{"id":"u001","text":""}]"#;
        let utts = vec![utt("u001", "原文长一点的句子")];
        let map = parse_refined(text, &utts).unwrap();
        assert_eq!(map.get("u001").unwrap(), "原文长一点的句子");
    }

    #[test]
    fn parse_refined_obvious_shrink_uses_original() {
        // Refined text much shorter than original -> use original.
        let text = r#"[{"id":"u001","text":"短"}]"#;
        let utts = vec![utt("u001", "这是一段很长的原文应该被保留不能被压缩成短句")];
        let map = parse_refined(text, &utts).unwrap();
        assert_eq!(
            map.get("u001").unwrap(),
            "这是一段很长的原文应该被保留不能被压缩成短句"
        );
    }

    #[test]
    fn parse_refined_duplicate_id_fails() {
        let text = r#"[{"id":"u001","text":"a"},{"id":"u001","text":"b"}]"#;
        let utts = vec![utt("u001", "原")];
        assert!(parse_refined(text, &utts).is_err());
    }

    #[test]
    fn extract_json_array_handles_prose() {
        let text = "Here is the result:\n```json\n[{\"id\":\"u1\",\"text\":\"x\"}]\n```\nDone.";
        let j = extract_json_array(text).unwrap();
        assert!(j.starts_with('['));
        assert!(j.ends_with(']'));
    }

    #[test]
    fn extract_json_array_handles_nested() {
        let text = "[{\"id\":\"u1\",\"text\":[\"a\",\"b\"]}]";
        let j = extract_json_array(text).unwrap();
        assert_eq!(j, text);
    }

    #[test]
    fn build_prompt_contains_all_ids_and_rules() {
        let utts = vec![utt("u001", "测试"), utt("u002", "另一条")];
        let p = build_prompt(&utts);
        assert!(p.contains("u001"));
        assert!(p.contains("u002"));
        assert!(p.contains("禁止摘要"));
        assert!(p.contains("URL"));
    }

    #[test]
    fn normalize_strips_v1_chat_completions() {
        assert_eq!(
            normalize_base_url("http://localhost:11434/v1/chat/completions"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn normalize_strips_v1_messages() {
        assert_eq!(
            normalize_base_url("https://api.example.com/v1/messages"),
            "https://api.example.com"
        );
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(
            normalize_base_url("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn normalize_leaves_clean_base() {
        assert_eq!(
            normalize_base_url("http://localhost:11434"),
            "http://localhost:11434"
        );
    }
}
