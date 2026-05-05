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
