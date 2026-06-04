use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::redact_json_value;
use crate::errors::{McpError, McpResult};
use crate::protocol::McpContent;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default, rename = "outputSchema")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub annotations: Option<Value>,
}

impl Default for McpToolDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    #[serde(default, rename = "structuredContent")]
    pub structured_content: Option<Value>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

pub fn mcp_result_to_text_and_metadata(
    result: &McpToolResult,
    max_bytes: usize,
) -> McpResult<(String, Value, usize, bool)> {
    let mut text_parts = Vec::new();
    let mut unsupported = Vec::new();
    let mut resource_links = Vec::new();
    let mut embedded_resources = Vec::new();

    for content in &result.content {
        match content {
            McpContent::Text { text, .. } => text_parts.push(text.clone()),
            McpContent::Image { mime_type, .. } => {
                unsupported.push(json!({"type": "image", "mime_type": mime_type}));
                text_parts.push("[unsupported MCP image content omitted]".into());
            }
            McpContent::Audio { mime_type, .. } => {
                unsupported.push(json!({"type": "audio", "mime_type": mime_type}));
                text_parts.push("[unsupported MCP audio content omitted]".into());
            }
            McpContent::ResourceLink {
                uri,
                name,
                mime_type,
                ..
            } => {
                resource_links.push(json!({
                    "uri": uri,
                    "name": name,
                    "mime_type": mime_type,
                }));
                text_parts.push(format!("Resource: {uri}"));
            }
            McpContent::EmbeddedResource { resource, .. } => {
                embedded_resources.push(json!({
                    "uri": resource.uri,
                    "mime_type": resource.mime_type,
                    "binary": resource.blob.is_some(),
                }));
                if let Some(t) = &resource.text {
                    text_parts.push(t.clone());
                } else {
                    unsupported
                        .push(json!({"type": "embedded_binary_resource", "uri": resource.uri}));
                    text_parts.push("[unsupported binary embedded resource omitted]".into());
                }
            }
            McpContent::Unknown(v) => {
                let handled = append_unknown_content(v, &mut text_parts, &mut unsupported);
                if !handled {
                    unsupported.push(json!({"type": "unknown", "content": redact_json_value(v)}));
                    text_parts.push("[unsupported MCP content omitted]".into());
                }
            }
        }
    }

    let joined = text_parts.join("\n");
    let original_bytes = joined.len();
    let (text, truncated) = truncate_utf8(&joined, max_bytes);
    let metadata = json!({
        "mcp": {
            "structured_content": result.structured_content.as_ref().map(redact_json_value),
            "is_error": result.is_error,
            "unsupported_content": unsupported,
            "resource_links": resource_links,
            "embedded_resources": embedded_resources,
            "truncated": truncated,
            "result_bytes": original_bytes,
        }
    });
    Ok((text, metadata, original_bytes, truncated))
}

fn append_unknown_content(
    v: &Value,
    text_parts: &mut Vec<String>,
    unsupported: &mut Vec<Value>,
) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(text) = obj.get("text").and_then(Value::as_str) {
                text_parts.push(text.to_string());
                return true;
            }
        }
        Some("image") => {
            unsupported.push(json!({"type": "image", "mime_type": obj.get("mimeType").or_else(|| obj.get("mime_type")).and_then(Value::as_str)}));
            text_parts.push("[unsupported MCP image content omitted]".into());
            return true;
        }
        Some("audio") => {
            unsupported.push(json!({"type": "audio", "mime_type": obj.get("mimeType").or_else(|| obj.get("mime_type")).and_then(Value::as_str)}));
            text_parts.push("[unsupported MCP audio content omitted]".into());
            return true;
        }
        Some("resource_link") | Some("resourceLink") => {
            if let Some(uri) = obj.get("uri").and_then(Value::as_str) {
                text_parts.push(format!("Resource: {uri}"));
                return true;
            }
        }
        Some("resource") => {
            if let Some(resource) = obj.get("resource").and_then(Value::as_object) {
                if let Some(text) = resource.get("text").and_then(Value::as_str) {
                    text_parts.push(text.to_string());
                } else {
                    unsupported.push(json!({"type": "embedded_binary_resource"}));
                    text_parts.push("[unsupported binary embedded resource omitted]".into());
                }
                return true;
            }
        }
        _ => {}
    }
    false
}

fn truncate_utf8(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    if max_bytes == 0 {
        return ("[MCP tool result truncated]".into(), true);
    }
    let marker = "\n[MCP tool result truncated]";
    let keep = max_bytes.saturating_sub(marker.len());
    let mut end = keep.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str(marker);
    (out, true)
}

pub fn enforce_non_binary_result_limit(actual: usize, limit: usize) -> McpResult<()> {
    if actual > limit.saturating_mul(4) {
        return Err(McpError::ResultTooLarge { actual, limit });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        AudioType, ImageType, McpContent, McpContentResource, ResourceLinkType, ResourceType,
        TextType,
    };

    fn text(s: &str) -> McpContent {
        McpContent::Text {
            content_type: TextType::Text,
            text: s.to_string(),
        }
    }

    fn result(content: Vec<McpContent>) -> McpToolResult {
        McpToolResult {
            content,
            structured_content: None,
            is_error: false,
            extra: serde_json::Map::new(),
        }
    }

    // ---- truncate_utf8 ----

    #[test]
    // A string within the byte cap is returned verbatim, untruncated.
    fn truncate_utf8_under_limit_is_unchanged() {
        let (out, truncated) = truncate_utf8("hello", 100);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[test]
    // A string exactly at the cap is not truncated (boundary is inclusive).
    fn truncate_utf8_at_exact_limit_is_unchanged() {
        let (out, truncated) = truncate_utf8("hello", 5);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[test]
    // A zero byte budget yields only the truncation marker, flagged truncated.
    fn truncate_utf8_zero_budget_returns_marker() {
        let (out, truncated) = truncate_utf8("hello", 0);
        assert_eq!(out, "[MCP tool result truncated]");
        assert!(truncated);
    }

    #[test]
    // Truncation never splits a multi-byte codepoint: the kept prefix is
    // backed off to a char boundary and the marker is appended.
    fn truncate_utf8_respects_char_boundary() {
        // Each `é` is 2 bytes; 10 of them = 20 bytes.
        let s = "é".repeat(10);
        let (out, truncated) = truncate_utf8(&s, 12);
        assert!(truncated);
        assert!(out.ends_with("[MCP tool result truncated]"));
        // The kept body must still be valid UTF-8 (no split codepoint).
        assert!(out.is_char_boundary(0));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    // ---- enforce_non_binary_result_limit ----

    #[test]
    // Sizes within the 4x envelope slack are accepted.
    fn enforce_limit_allows_up_to_four_x() {
        assert!(enforce_non_binary_result_limit(400, 100).is_ok());
    }

    #[test]
    // One byte past 4x the structured limit is rejected as ResultTooLarge,
    // preserving the original (non-multiplied) limit in the error.
    fn enforce_limit_rejects_past_four_x() {
        let err = enforce_non_binary_result_limit(401, 100).unwrap_err();
        match err {
            McpError::ResultTooLarge { actual, limit } => {
                assert_eq!(actual, 401);
                assert_eq!(limit, 100);
            }
            other => panic!("expected ResultTooLarge, got {other:?}"),
        }
    }

    // ---- mcp_result_to_text_and_metadata ----

    #[test]
    // Multiple text parts are newline-joined and reported with their byte size.
    fn shapes_multiple_text_parts() {
        let r = result(vec![text("alpha"), text("beta")]);
        let (txt, meta, bytes, truncated) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert_eq!(txt, "alpha\nbeta");
        assert_eq!(bytes, "alpha\nbeta".len());
        assert!(!truncated);
        assert_eq!(meta["mcp"]["is_error"], false);
        assert_eq!(meta["mcp"]["truncated"], false);
    }

    #[test]
    // Image content is omitted from text but recorded under unsupported_content.
    fn image_content_is_unsupported() {
        let r = result(vec![McpContent::Image {
            content_type: ImageType::Image,
            data: "AAAA".into(),
            mime_type: "image/png".into(),
        }]);
        let (txt, meta, _, _) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert!(txt.contains("[unsupported MCP image content omitted]"));
        let unsupported = meta["mcp"]["unsupported_content"].as_array().unwrap();
        assert_eq!(unsupported.len(), 1);
        assert_eq!(unsupported[0]["type"], "image");
        assert_eq!(unsupported[0]["mime_type"], "image/png");
    }

    #[test]
    // Audio content behaves like image: placeholder text + unsupported entry.
    fn audio_content_is_unsupported() {
        let r = result(vec![McpContent::Audio {
            content_type: AudioType::Audio,
            data: "AAAA".into(),
            mime_type: "audio/wav".into(),
        }]);
        let (txt, meta, _, _) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert!(txt.contains("[unsupported MCP audio content omitted]"));
        assert_eq!(meta["mcp"]["unsupported_content"][0]["type"], "audio");
    }

    #[test]
    // A resource link emits a `Resource: <uri>` line and a metadata entry.
    fn resource_link_is_recorded() {
        let r = result(vec![McpContent::ResourceLink {
            content_type: ResourceLinkType::ResourceLink,
            uri: "file:///x".into(),
            name: Some("x".into()),
            mime_type: Some("text/plain".into()),
        }]);
        let (txt, meta, _, _) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert!(txt.contains("Resource: file:///x"));
        assert_eq!(meta["mcp"]["resource_links"][0]["uri"], "file:///x");
    }

    #[test]
    // A text-bearing embedded resource contributes its text inline.
    fn embedded_text_resource_inlines_text() {
        let r = result(vec![McpContent::EmbeddedResource {
            content_type: ResourceType::Resource,
            resource: McpContentResource {
                uri: "file:///doc".into(),
                text: Some("body".into()),
                blob: None,
                mime_type: Some("text/plain".into()),
            },
        }]);
        let (txt, meta, _, _) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert!(txt.contains("body"));
        assert_eq!(meta["mcp"]["embedded_resources"][0]["binary"], false);
    }

    #[test]
    // A binary-only embedded resource is omitted with a placeholder and flagged.
    fn embedded_binary_resource_is_unsupported() {
        let r = result(vec![McpContent::EmbeddedResource {
            content_type: ResourceType::Resource,
            resource: McpContentResource {
                uri: "file:///bin".into(),
                text: None,
                blob: Some("AAAA".into()),
                mime_type: Some("application/octet-stream".into()),
            },
        }]);
        let (txt, meta, _, _) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert!(txt.contains("[unsupported binary embedded resource omitted]"));
        assert_eq!(meta["mcp"]["embedded_resources"][0]["binary"], true);
        assert_eq!(
            meta["mcp"]["unsupported_content"][0]["type"],
            "embedded_binary_resource"
        );
    }

    #[test]
    // is_error from the tool result is propagated into metadata.
    fn is_error_is_propagated() {
        let mut r = result(vec![text("boom")]);
        r.is_error = true;
        let (_, meta, _, _) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert_eq!(meta["mcp"]["is_error"], true);
    }

    #[test]
    // When the joined text exceeds max_bytes it is truncated and flagged, but
    // result_bytes still reports the original (pre-truncation) size.
    fn long_text_is_truncated_with_original_byte_count() {
        let r = result(vec![text(&"x".repeat(500))]);
        let (txt, meta, bytes, truncated) = mcp_result_to_text_and_metadata(&r, 64).unwrap();
        assert!(truncated);
        assert!(txt.len() <= 64);
        assert_eq!(bytes, 500);
        assert_eq!(meta["mcp"]["truncated"], true);
        assert_eq!(meta["mcp"]["result_bytes"], 500);
    }

    // ---- append_unknown_content (via the Unknown content branch) ----

    #[test]
    // An Unknown object tagged `type: text` is recovered as inline text.
    fn unknown_text_content_is_recovered() {
        let r = result(vec![McpContent::Unknown(json!({
            "type": "text",
            "text": "recovered"
        }))]);
        let (txt, meta, _, _) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert_eq!(txt, "recovered");
        assert!(meta["mcp"]["unsupported_content"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    // A fully unrecognized Unknown content shape is omitted and flagged.
    fn unknown_unrecognized_content_is_unsupported() {
        let r = result(vec![McpContent::Unknown(json!({
            "type": "widget",
            "payload": 1
        }))]);
        let (txt, meta, _, _) = mcp_result_to_text_and_metadata(&r, 1024).unwrap();
        assert!(txt.contains("[unsupported MCP content omitted]"));
        assert_eq!(meta["mcp"]["unsupported_content"][0]["type"], "unknown");
    }

    #[test]
    // append_unknown_content returns false for non-object values, leaving the
    // caller to record them as generic unsupported content.
    fn append_unknown_content_rejects_non_object() {
        let mut parts = Vec::new();
        let mut unsupported = Vec::new();
        let handled = append_unknown_content(&json!("scalar"), &mut parts, &mut unsupported);
        assert!(!handled);
        assert!(parts.is_empty());
    }
}
