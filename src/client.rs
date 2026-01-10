use crate::error::Result;
use crate::message::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, MessageWithFds};
use crate::transport::{Sender, UnixSocketTransport};
use serde_json::Value;
use std::os::unix::io::OwnedFd;
use std::path::Path;
use tokio::net::UnixStream;
use tracing::debug;

pub struct Client {
    sender: Sender,
    next_id: u64,
}

impl Client {
    pub async fn connect<P: AsRef<Path>>(path: P) -> Result<Self> {
        let stream = UnixStream::connect(path).await?;
        let transport = UnixSocketTransport::new(stream)?;
        let (sender, _receiver) = transport.split();

        Ok(Self { sender, next_id: 1 })
    }

    pub async fn call_method(
        &mut self,
        method: &str,
        params: Option<Value>,
        file_descriptors: Vec<OwnedFd>,
    ) -> Result<()> {
        let id = Value::Number(self.next_id.into());
        self.next_id += 1;

        let request = JsonRpcRequest::new(method.to_string(), params, id);
        let message = JsonRpcMessage::Request(request);
        let message_with_fds = MessageWithFds::new(message, file_descriptors);

        debug!("Sending method call: {}", method);
        self.sender.send(message_with_fds).await
    }

    pub async fn send_notification(
        &mut self,
        method: &str,
        params: Option<Value>,
        file_descriptors: Vec<OwnedFd>,
    ) -> Result<()> {
        let notification = JsonRpcNotification::new(method.to_string(), params);
        let message = JsonRpcMessage::Notification(notification);
        let message_with_fds = MessageWithFds::new(message, file_descriptors);

        debug!("Sending notification: {}", method);
        self.sender.send(message_with_fds).await
    }
}
