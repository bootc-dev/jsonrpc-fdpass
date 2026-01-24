use crate::error::{Error, Result};
use jsonrpsee::types::error::ErrorObject as JsonRpcError;
use serde::{Deserialize, Serialize};
use std::os::unix::io::OwnedFd;

/// The JSON key for the file descriptor count field.
pub const FDS_KEY: &str = "fds";
/// The JSON-RPC protocol version.
pub const JSONRPC_VERSION: &str = "2.0";

/// Read the file descriptor count from a JSON message.
/// Returns 0 if the `fds` field is absent.
pub fn get_fd_count(value: &serde_json::Value) -> usize {
    value
        .get(FDS_KEY)
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(0)
}

/// Helper to skip serializing fds field when it's None or 0
fn skip_if_zero_or_none(fds: &Option<usize>) -> bool {
    fds.map_or(true, |n| n == 0)
}

// Define our own message types that don't have lifetime constraints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    pub id: serde_json::Value,
    /// Number of file descriptors attached to this message
    #[serde(skip_serializing_if = "skip_if_zero_or_none")]
    pub fds: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError<'static>>,
    pub id: serde_json::Value,
    /// Number of file descriptors attached to this message
    #[serde(skip_serializing_if = "skip_if_zero_or_none")]
    pub fds: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// Number of file descriptors attached to this message
    #[serde(skip_serializing_if = "skip_if_zero_or_none")]
    pub fds: Option<usize>,
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
            fds: None,
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
            fds: None,
        }
    }

    pub fn error(error: JsonRpcError<'static>, id: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            result: None,
            error: Some(error),
            id,
            fds: None,
        }
    }
}

impl JsonRpcNotification {
    pub fn new(method: String, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method,
            params,
            fds: None,
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

impl JsonRpcMessage {
    /// Set the fds count on the message
    pub fn set_fds(&mut self, count: usize) {
        let fds = if count > 0 { Some(count) } else { None };
        match self {
            JsonRpcMessage::Request(req) => req.fds = fds,
            JsonRpcMessage::Response(res) => res.fds = fds,
            JsonRpcMessage::Notification(notif) => notif.fds = fds,
        }
    }

    /// Get the fds count from the message
    pub fn get_fds(&self) -> usize {
        match self {
            JsonRpcMessage::Request(req) => req.fds.unwrap_or(0),
            JsonRpcMessage::Response(res) => res.fds.unwrap_or(0),
            JsonRpcMessage::Notification(notif) => notif.fds.unwrap_or(0),
        }
    }
}

impl MessageWithFds {
    pub fn new(message: JsonRpcMessage, file_descriptors: Vec<OwnedFd>) -> Self {
        Self {
            message,
            file_descriptors,
        }
    }

    /// Serialize the message, setting the `fds` field to match the number of attached FDs.
    pub fn serialize(&self) -> Result<String> {
        self.serialize_impl(false)
    }

    /// Serialize the message with pretty-printing.
    pub fn serialize_pretty(&self) -> Result<String> {
        self.serialize_impl(true)
    }

    fn serialize_impl(&self, pretty: bool) -> Result<String> {
        // Clone the message so we can set the fds field
        let mut message = self.message.clone();
        message.set_fds(self.file_descriptors.len());

        let message_json = message.to_json_value()?;
        let json_str = if pretty {
            serde_json::to_string_pretty(&message_json)?
        } else {
            serde_json::to_string(&message_json)?
        };
        Ok(json_str)
    }

    /// Create a MessageWithFds from parsed JSON and file descriptors.
    /// The `fds` field in the JSON must match the number of provided FDs.
    pub fn from_json_with_fds(json_str: &str, fds: Vec<OwnedFd>) -> Result<Self> {
        let message_json: serde_json::Value = serde_json::from_str(json_str)?;
        let expected_count = get_fd_count(&message_json);

        if expected_count != fds.len() {
            return Err(Error::MismatchedCount {
                expected: expected_count,
                found: fds.len(),
            });
        }

        let message = JsonRpcMessage::from_json_value(message_json)?;
        Ok(Self::new(message, fds))
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
