use crate::error::{Error, Result};
use crate::message::{count_fd_placeholders, JsonRpcMessage, MessageWithFds};
use rustix::fd::AsFd;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut};
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use tokio::io::Interest;
use tokio::net::UnixStream as TokioUnixStream;
use tracing::{debug, trace};

/// Maximum number of file descriptors per message.
const MAX_FDS_PER_MESSAGE: usize = 8;
/// Read buffer size for incoming data.
const READ_BUFFER_SIZE: usize = 4096;

pub struct UnixSocketTransport {
    stream: TokioUnixStream,
}

impl UnixSocketTransport {
    pub fn new(stream: TokioUnixStream) -> Result<Self> {
        Ok(Self { stream })
    }

    pub fn split(self) -> (Sender, Receiver) {
        let stream = Arc::new(self.stream);

        (
            Sender {
                stream: Arc::clone(&stream),
                pretty: false,
            },
            Receiver {
                stream,
                buffer: Vec::new(),
                fd_queue: VecDeque::new(),
            },
        )
    }
}

pub struct Sender {
    stream: Arc<TokioUnixStream>,
    pretty: bool,
}

impl Sender {
    /// Enable or disable pretty-printed JSON output.
    ///
    /// When enabled, messages are serialized with indentation and newlines.
    /// This is useful for debugging or when interoperating with tools that
    /// expect human-readable JSON.
    pub fn set_pretty(&mut self, pretty: bool) {
        self.pretty = pretty;
    }

    pub async fn send(&mut self, message_with_fds: MessageWithFds) -> Result<()> {
        let serialized = if self.pretty {
            message_with_fds.serialize_with_placeholders_pretty()?
        } else {
            message_with_fds.serialize_with_placeholders()?
        };
        let data = serialized.into_bytes();

        trace!(
            "Sending message: {} with {} FDs",
            String::from_utf8_lossy(&data).trim(),
            message_with_fds.file_descriptors.len()
        );

        let fds = message_with_fds.file_descriptors;

        self.stream
            .async_io(Interest::WRITABLE, || {
                let sockfd = self.stream.as_fd();

                if fds.is_empty() {
                    // No file descriptors to send - use regular send
                    rustix::net::send(sockfd, &data, SendFlags::empty())
                        .map_err(|e| to_io_error(e, "send"))?;
                } else {
                    // Convert OwnedFd to BorrowedFd for sending
                    let borrowed_fds: Vec<_> = fds.iter().map(|fd| fd.as_fd()).collect();

                    let mut buffer: Vec<u8> =
                        vec![0u8; rustix::cmsg_space!(ScmRights(MAX_FDS_PER_MESSAGE))];
                    let mut control = SendAncillaryBuffer::new(buffer.as_mut_slice());

                    if !control.push(SendAncillaryMessage::ScmRights(&borrowed_fds)) {
                        return Err(io::Error::other(
                            "Failed to add file descriptors to control message",
                        ));
                    }

                    let iov = [IoSlice::new(&data)];
                    rustix::net::sendmsg(sockfd, &iov, &mut control, SendFlags::empty())
                        .map_err(|e| to_io_error(e, "sendmsg"))?;
                }
                Ok(())
            })
            .await
            .map_err(Error::Io)
    }
}

pub struct Receiver {
    stream: Arc<TokioUnixStream>,
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
        if self.buffer.is_empty() {
            return Ok(None);
        }

        // Use streaming JSON parser to find message boundaries
        let mut stream =
            serde_json::Deserializer::from_slice(&self.buffer).into_iter::<serde_json::Value>();

        match stream.next() {
            Some(Ok(value)) => {
                // Successfully parsed a complete JSON value
                let bytes_consumed = stream.byte_offset();

                trace!("Parsed message ({} bytes): {:?}", bytes_consumed, value);

                // Drain the consumed bytes from the buffer
                self.buffer.drain(..bytes_consumed);

                // Count placeholders and extract FDs
                let placeholder_count = count_fd_placeholders(&value);

                if placeholder_count > self.fd_queue.len() {
                    return Err(Error::MismatchedCount {
                        expected: placeholder_count,
                        found: self.fd_queue.len(),
                    });
                }

                let fds: Vec<OwnedFd> = (0..placeholder_count)
                    .map(|_| self.fd_queue.pop_front().unwrap())
                    .collect();

                let message = JsonRpcMessage::from_json_value(value)?;
                Ok(Some(MessageWithFds::new(message, fds)))
            }
            Some(Err(e)) if e.is_eof() => {
                // Incomplete JSON - need more data
                Ok(None)
            }
            Some(Err(e)) => {
                // Actual parse error
                Err(Error::Json(e))
            }
            None => {
                // No more values (shouldn't happen with non-empty buffer, but handle it)
                Ok(None)
            }
        }
    }

    async fn read_more_data(&mut self) -> Result<()> {
        let mut data_buffer = [0u8; READ_BUFFER_SIZE];
        let mut received_fds: Vec<OwnedFd> = Vec::new();

        let bytes_read = self
            .stream
            .async_io(Interest::READABLE, || {
                let sockfd = self.stream.as_fd();

                let mut iov = [IoSliceMut::new(&mut data_buffer)];
                let mut cmsg_space: Vec<u8> =
                    vec![0u8; rustix::cmsg_space!(ScmRights(MAX_FDS_PER_MESSAGE))];
                let mut cmsg_buffer = RecvAncillaryBuffer::new(cmsg_space.as_mut_slice());

                let result = rustix::net::recvmsg(
                    sockfd,
                    &mut iov,
                    &mut cmsg_buffer,
                    RecvFlags::CMSG_CLOEXEC,
                )
                .map_err(|e| to_io_error(e, "recvmsg"))?;

                // Extract file descriptors from control messages
                for msg in cmsg_buffer.drain() {
                    if let RecvAncillaryMessage::ScmRights(fds) = msg {
                        received_fds.extend(fds);
                    }
                }

                Ok(result.bytes)
            })
            .await
            .map_err(Error::Io)?;

        if bytes_read == 0 {
            return Err(Error::ConnectionClosed);
        }

        self.buffer.extend_from_slice(&data_buffer[..bytes_read]);
        self.fd_queue.extend(received_fds);

        debug!(
            "Read {} bytes, {} FDs in queue",
            bytes_read,
            self.fd_queue.len()
        );
        Ok(())
    }
}

/// Convert a rustix error to an io::Error, preserving EAGAIN/EWOULDBLOCK for async_io
fn to_io_error(e: rustix::io::Errno, operation: &str) -> io::Error {
    // rustix::io::Errno can be converted to io::Error, which preserves the error kind
    let io_err: io::Error = e.into();
    if io_err.kind() == io::ErrorKind::WouldBlock {
        io_err
    } else {
        io::Error::new(io_err.kind(), format!("{} failed: {}", operation, io_err))
    }
}
