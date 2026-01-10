use crate::error::{Error, Result};
use jsonrpsee::types::error::ErrorObject as JsonRpcError;
use serde::{Deserialize, Serialize};
use std::os::unix::io::OwnedFd;

/// The JSON key used to identify file descriptor placeholders.
pub const FD_PLACEHOLDER_KEY: &str = "__jsonrpc_fd__";
/// The JSON key for the file descriptor index within a placeholder.
pub const FD_INDEX_KEY: &str = "index";
/// The JSON-RPC protocol version.
pub const JSONRPC_VERSION: &str = "2.0";

/// Count file descriptor placeholders in a JSON value.
pub fn count_fd_placeholders(value: &serde_json::Value) -> usize {
    fn count_inner(value: &serde_json::Value, count: &mut usize) {
        match value {
            serde_json::Value::Object(map) => {
                if let (Some(serde_json::Value::Bool(true)), Some(_)) =
                    (map.get(FD_PLACEHOLDER_KEY), map.get(FD_INDEX_KEY))
                {
                    *count += 1;
                } else {
                    for v in map.values() {
                        count_inner(v, count);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    count_inner(v, count);
                }
            }
            _ => {}
        }
    }
    let mut count = 0;
    count_inner(value, &mut count);
    count
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDescriptorPlaceholder {
    #[serde(rename = "__jsonrpc_fd__")]
    pub marker: bool,
    pub index: usize,
}

impl FileDescriptorPlaceholder {
    pub fn new(index: usize) -> Self {
        Self {
            marker: true,
            index,
        }
    }
}

// Define our own message types that don't have lifetime constraints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    pub params: Option<serde_json::Value>,
    pub id: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError<'static>>,
    pub id: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

impl JsonRpcRequest {
    pub fn new(method: String, params: Option<serde_json::Value>, id: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method,
            params,
            id,
        }
    }
}

impl JsonRpcResponse {
    pub fn success(result: serde_json::Value, id: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn error(error: JsonRpcError<'static>, id: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            result: None,
            error: Some(error),
            id,
        }
    }
}

impl JsonRpcNotification {
    pub fn new(method: String, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method,
            params,
        }
    }
}

impl JsonRpcMessage {
    pub fn to_json_value(&self) -> Result<serde_json::Value> {
        match self {
            JsonRpcMessage::Request(req) => Ok(serde_json::to_value(req)?),
            JsonRpcMessage::Response(res) => Ok(serde_json::to_value(res)?),
            JsonRpcMessage::Notification(notif) => Ok(serde_json::to_value(notif)?),
        }
    }

    pub fn from_json_value(value: serde_json::Value) -> Result<Self> {
        if let serde_json::Value::Object(obj) = &value {
            if obj.contains_key("method") && obj.contains_key("id") {
                let request: JsonRpcRequest = serde_json::from_value(value)?;
                Ok(JsonRpcMessage::Request(request))
            } else if obj.contains_key("result") || obj.contains_key("error") {
                let response: JsonRpcResponse = serde_json::from_value(value)?;
                Ok(JsonRpcMessage::Response(response))
            } else if obj.contains_key("method") {
                let notification: JsonRpcNotification = serde_json::from_value(value)?;
                Ok(JsonRpcMessage::Notification(notification))
            } else {
                Err(Error::InvalidMessage("Invalid JSON-RPC message".into()))
            }
        } else {
            Err(Error::InvalidMessage("Expected JSON object".into()))
        }
    }
}

#[derive(Debug)]
pub struct MessageWithFds {
    pub message: JsonRpcMessage,
    pub file_descriptors: Vec<OwnedFd>,
}

impl MessageWithFds {
    pub fn new(message: JsonRpcMessage, file_descriptors: Vec<OwnedFd>) -> Self {
        Self {
            message,
            file_descriptors,
        }
    }

    pub fn serialize_with_placeholders(&self) -> Result<String> {
        let mut message_json = self.message.to_json_value()?;
        self.insert_placeholders(&mut message_json)?;

        let json_str = serde_json::to_string(&message_json)?;
        Ok(json_str)
    }

    fn insert_placeholders(&self, value: &mut serde_json::Value) -> Result<()> {
        let fd_count = self.file_descriptors.len();
        let mut placeholder_indices = Vec::new();

        Self::collect_placeholder_indices(value, &mut placeholder_indices);

        if placeholder_indices.len() != fd_count {
            return Err(Error::MismatchedCount {
                expected: fd_count,
                found: placeholder_indices.len(),
            });
        }

        placeholder_indices.sort_unstable();
        let expected: Vec<usize> = (0..fd_count).collect();
        if placeholder_indices != expected {
            return Err(Error::InvalidPlaceholders);
        }

        Ok(())
    }

    fn collect_placeholder_indices(value: &serde_json::Value, indices: &mut Vec<usize>) {
        match value {
            serde_json::Value::Object(map) => {
                if let (
                    Some(serde_json::Value::Bool(true)),
                    Some(serde_json::Value::Number(index)),
                ) = (map.get(FD_PLACEHOLDER_KEY), map.get(FD_INDEX_KEY))
                {
                    if let Some(index) = index.as_u64() {
                        indices.push(index as usize);
                    }
                } else {
                    for v in map.values() {
                        Self::collect_placeholder_indices(v, indices);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    Self::collect_placeholder_indices(v, indices);
                }
            }
            _ => {}
        }
    }

    pub fn from_json_with_fds(json_str: &str, fds: Vec<OwnedFd>) -> Result<Self> {
        let message_json: serde_json::Value = serde_json::from_str(json_str)?;

        let placeholder_count = count_fd_placeholders(&message_json);

        if placeholder_count != fds.len() {
            return Err(Error::MismatchedCount {
                expected: placeholder_count,
                found: fds.len(),
            });
        }

        if placeholder_count > 0 {
            Self::validate_placeholder_indices(&message_json, placeholder_count)?;
        } else if !fds.is_empty() {
            return Err(Error::DanglingFileDescriptors);
        }

        let message = JsonRpcMessage::from_json_value(message_json)?;
        Ok(Self::new(message, fds))
    }

    fn validate_placeholder_indices(
        value: &serde_json::Value,
        expected_count: usize,
    ) -> Result<()> {
        let mut indices = Vec::new();
        Self::collect_indices(value, &mut indices);

        indices.sort_unstable();
        let expected: Vec<usize> = (0..expected_count).collect();

        if indices != expected {
            return Err(Error::InvalidPlaceholders);
        }

        Ok(())
    }

    fn collect_indices(value: &serde_json::Value, indices: &mut Vec<usize>) {
        match value {
            serde_json::Value::Object(map) => {
                if let (
                    Some(serde_json::Value::Bool(true)),
                    Some(serde_json::Value::Number(index)),
                ) = (map.get(FD_PLACEHOLDER_KEY), map.get(FD_INDEX_KEY))
                {
                    if let Some(index) = index.as_u64() {
                        indices.push(index as usize);
                    }
                } else {
                    for v in map.values() {
                        Self::collect_indices(v, indices);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    Self::collect_indices(v, indices);
                }
            }
            _ => {}
        }
    }
}

pub const FILE_DESCRIPTOR_ERROR_CODE: i32 = -32050;

pub fn file_descriptor_error() -> JsonRpcError<'static> {
    JsonRpcError::owned(
        FILE_DESCRIPTOR_ERROR_CODE,
        "File Descriptor Error",
        None::<serde_json::Value>,
    )
}
