use crate::error::{Error, Result};
use crate::message::{JsonRpcMessage, MessageWithFds, FD_INDEX_KEY, FD_PLACEHOLDER_KEY};
use rustix::fd::AsFd;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use std::collections::VecDeque;
use std::io::{IoSlice, IoSliceMut};
use std::os::unix::io::OwnedFd;
use std::sync::{Arc, Mutex};
use tokio::net::UnixStream as TokioUnixStream;
use tracing::{debug, trace};

pub struct UnixSocketTransport {
    fd: OwnedFd,
}

impl UnixSocketTransport {
    pub fn new(stream: TokioUnixStream) -> Self {
        // Convert tokio stream to OwnedFd
        let fd = stream.into_std().unwrap().into();
        Self { fd }
    }

    pub fn split(self) -> (Sender, Receiver) {
        let fd = Arc::new(Mutex::new(self.fd));

        (
            Sender {
                fd: Arc::clone(&fd),
            },
            Receiver {
                fd: Arc::clone(&fd),
                buffer: Vec::new(),
                fd_queue: VecDeque::new(),
            },
        )
    }
}

pub struct Sender {
    fd: Arc<Mutex<OwnedFd>>,
}

impl Sender {
    pub async fn send(&mut self, message_with_fds: MessageWithFds) -> Result<()> {
        let serialized = message_with_fds.serialize_with_placeholders()?;
        let data = serialized.into_bytes();

        trace!(
            "Sending message: {} with {} FDs",
            String::from_utf8_lossy(&data).trim(),
            message_with_fds.file_descriptors.len()
        );

        let fd = Arc::clone(&self.fd);
        let fds = message_with_fds.file_descriptors;

        tokio::task::spawn_blocking(move || {
            let sockfd = fd.lock().unwrap();

            if fds.is_empty() {
                // No file descriptors to send - use regular send
                rustix::net::send(&*sockfd, &data, SendFlags::empty())
                    .map_err(|e| Error::SystemCall(format!("send failed: {}", e)))?;
            } else {
                // Convert OwnedFd to BorrowedFd for sending
                let borrowed_fds: Vec<_> = fds.iter().map(|fd| fd.as_fd()).collect();

                let mut buffer: Vec<u8> = vec![0u8; rustix::cmsg_space!(ScmRights(8))];
                let mut control = SendAncillaryBuffer::new(buffer.as_mut_slice());

                if !control.push(SendAncillaryMessage::ScmRights(&borrowed_fds)) {
                    return Err(Error::SystemCall(
                        "Failed to add file descriptors to control message".to_string(),
                    ));
                }

                let iov = [IoSlice::new(&data)];
                rustix::net::sendmsg(&*sockfd, &iov, &mut control, SendFlags::empty())
                    .map_err(|e| Error::SystemCall(format!("sendmsg failed: {}", e)))?;
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::SystemCall(format!("Task join error: {}", e)))?
    }
}

pub struct Receiver {
    fd: Arc<Mutex<OwnedFd>>,
    buffer: Vec<u8>,
    fd_queue: VecDeque<OwnedFd>,
}

impl Receiver {
    pub async fn receive(&mut self) -> Result<MessageWithFds> {
        loop {
            if let Some(message) = self.try_parse_message()? {
                return Ok(message);
            }

            self.read_more_data().await?;
        }
    }

    fn try_parse_message(&mut self) -> Result<Option<MessageWithFds>> {
        if let Some(newline_pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let message_bytes = self.buffer.drain(..=newline_pos).collect::<Vec<u8>>();
            let message_str = std::str::from_utf8(&message_bytes[..message_bytes.len() - 1])
                .map_err(|_| Error::FramingError)?;

            trace!("Parsing message: {}", message_str);

            let mut placeholder_count = 0;
            Self::count_placeholders_in_str(message_str, &mut placeholder_count)?;

            if placeholder_count > self.fd_queue.len() {
                return Err(Error::MismatchedCount {
                    expected: placeholder_count,
                    found: self.fd_queue.len(),
                });
            }

            let fds: Vec<OwnedFd> = (0..placeholder_count)
                .map(|_| self.fd_queue.pop_front().unwrap())
                .collect();

            if placeholder_count == 0 && !fds.is_empty() {
                return Err(Error::DanglingFileDescriptors);
            }

            let message = self.parse_json_message(message_str)?;
            Ok(Some(MessageWithFds::new(message, fds)))
        } else {
            Ok(None)
        }
    }

    fn parse_json_message(&self, json_str: &str) -> Result<JsonRpcMessage> {
        let value: serde_json::Value = serde_json::from_str(json_str)?;
        JsonRpcMessage::from_json_value(value)
    }

    fn count_placeholders_in_str(json_str: &str, count: &mut usize) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(json_str)?;
        Self::count_placeholders(&value, count);
        Ok(())
    }

    fn count_placeholders(value: &serde_json::Value, count: &mut usize) {
        match value {
            serde_json::Value::Object(map) => {
                if let (Some(serde_json::Value::Bool(true)), Some(_)) =
                    (map.get(FD_PLACEHOLDER_KEY), map.get(FD_INDEX_KEY))
                {
                    *count += 1;
                } else {
                    for v in map.values() {
                        Self::count_placeholders(v, count);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    Self::count_placeholders(v, count);
                }
            }
            _ => {}
        }
    }

    async fn read_more_data(&mut self) -> Result<()> {
        let fd = Arc::clone(&self.fd);

        let (bytes_read, data, fds) = tokio::task::spawn_blocking(move || {
            let sockfd = fd.lock().unwrap();
            let mut data_buffer = [0u8; 4096];
            let mut iov = [IoSliceMut::new(&mut data_buffer)];
            let mut cmsg_space: Vec<u8> = vec![0u8; rustix::cmsg_space!(ScmRights(8))];
            let mut cmsg_buffer = RecvAncillaryBuffer::new(cmsg_space.as_mut_slice());

            let result = rustix::net::recvmsg(
                &*sockfd,
                &mut iov,
                &mut cmsg_buffer,
                RecvFlags::CMSG_CLOEXEC,
            )
            .map_err(|e| Error::SystemCall(format!("recvmsg failed: {}", e)))?;

            let bytes_read = result.bytes;
            let mut fds = Vec::new();

            // Extract file descriptors from control messages
            for msg in cmsg_buffer.drain() {
                if let RecvAncillaryMessage::ScmRights(received_fds) = msg {
                    fds.extend(received_fds);
                }
            }

            Ok::<_, Error>((bytes_read, data_buffer, fds))
        })
        .await
        .map_err(|e| Error::SystemCall(format!("Task join error: {}", e)))??;

        if bytes_read == 0 {
            return Err(Error::ConnectionClosed);
        }

        self.buffer.extend_from_slice(&data[..bytes_read]);
        self.fd_queue.extend(fds);

        debug!(
            "Read {} bytes, {} FDs in queue",
            bytes_read,
            self.fd_queue.len()
        );
        Ok(())
    }
}
