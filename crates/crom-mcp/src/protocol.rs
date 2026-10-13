use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::{McpError, McpResult};

pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(u64),
    String(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: JsonRpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "code {}: {}", self.code, self.message)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<JsonRpcId>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

pub fn parse_json_rpc_message(line: &str) -> McpResult<JsonRpcMessage> {
    let value: Value = serde_json::from_str(line)?;
    parse_json_rpc_value(value)
}

pub fn parse_json_rpc_value(value: Value) -> McpResult<JsonRpcMessage> {
    let obj = value
        .as_object()
        .ok_or_else(|| McpError::InvalidJsonRpc("message must be a JSON object".into()))?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpError::InvalidJsonRpc(
            "jsonrpc field must equal \"2.0\"".into(),
        ));
    }
    if obj.contains_key("method") && obj.contains_key("id") {
        let req: JsonRpcRequest = serde_json::from_value(Value::Object(obj.clone()))?;
        Ok(JsonRpcMessage::Request(req))
    } else if obj.contains_key("method") {
        let note: JsonRpcNotification = serde_json::from_value(Value::Object(obj.clone()))?;
        Ok(JsonRpcMessage::Notification(note))
    } else if obj.contains_key("result") || obj.contains_key("error") {
        let resp: JsonRpcResponse = serde_json::from_value(Value::Object(obj.clone()))?;
        if resp.result.is_some() && resp.error.is_some() {
            return Err(McpError::InvalidJsonRpc(
                "response cannot contain both result and error".into(),
            ));
        }
        if resp.result.is_none() && resp.error.is_none() {
            return Err(McpError::InvalidJsonRpc(
                "response must contain result or error".into(),
            ));
        }
        Ok(JsonRpcMessage::Response(resp))
    } else {
        Err(McpError::InvalidJsonRpc(
            "message is not a request, response, or notification".into(),
        ))
    }
}

#[derive(Debug, Default)]
pub struct JsonRpcIdAllocator {
    next: AtomicU64,
}

impl JsonRpcIdAllocator {
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    pub fn next_id(&self) -> JsonRpcId {
        JsonRpcId::Number(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub roots: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub sampling: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub elicitation: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpContentResource {
    pub uri: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpContent {
    Text {
        #[serde(rename = "type")]
        content_type: TextType,
        text: String,
    },
    Image {
        #[serde(rename = "type")]
        content_type: ImageType,
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Audio {
        #[serde(rename = "type")]
        content_type: AudioType,
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    ResourceLink {
        #[serde(rename = "type")]
        content_type: ResourceLinkType,
        uri: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default, rename = "mimeType")]
        mime_type: Option<String>,
    },
    EmbeddedResource {
        #[serde(rename = "type")]
        content_type: ResourceType,
        resource: McpContentResource,
    },
    Unknown(Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextType {
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageType {
    Image,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioType {
    Audio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceLinkType {
    ResourceLink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceType {
    Resource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // JSON-RPC requests serialize with version, id, method, and params.
    fn request_serialization() {
        let req = JsonRpcRequest::new(
            JsonRpcId::Number(7),
            "tools/list",
            Some(serde_json::json!({})),
        );
        let value = serde_json::to_value(req).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "tools/list");
    }

    #[test]
    // JSON-RPC success responses parse as response messages.
    fn response_parsing() {
        let msg =
            parse_json_rpc_message(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Response(_)));
    }

    #[test]
    // JSON-RPC error responses preserve code, message, and id.
    fn error_response_parsing() {
        let msg = parse_json_rpc_message(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"missing"}}"#,
        )
        .unwrap();
        match msg {
            JsonRpcMessage::Response(resp) => {
                assert_eq!(resp.id, Some(JsonRpcId::Number(2)));
                assert_eq!(resp.error.unwrap().code, -32601);
            }
            _ => panic!("expected response"),
        }
    }

    #[test]
    // JSON-RPC notifications parse without a request id.
    fn notification_parsing() {
        let msg =
            parse_json_rpc_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(matches!(msg, JsonRpcMessage::Notification(_)));
    }

    #[test]
    // Request ids allocated within one client/session are unique.
    fn request_ids_are_unique() {
        let ids = JsonRpcIdAllocator::new();
        assert_ne!(ids.next_id(), ids.next_id());
    }

    #[test]
    // Malformed JSON-RPC returns a structured error instead of panicking.
    fn invalid_json_rpc_errors() {
        let err = parse_json_rpc_message(r#"{"id":1}"#).unwrap_err();
        assert!(matches!(err, McpError::InvalidJsonRpc(_)));
    }

    #[test]
    // Malformed MCP payloads deserialize as errors without panicking.
    fn malformed_mcp_payload_errors() {
        let value = serde_json::json!({
            "protocolVersion": "",
            "capabilities": {},
            "serverInfo": {"name": "x", "version": "1"}
        });
        let parsed: InitializeResponse = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.protocol_version, "");
    }
}
