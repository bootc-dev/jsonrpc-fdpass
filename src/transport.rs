use crate::error::{Error, Result};
use crate::message::{get_fd_count, JsonRpcMessage, JsonRpcNotification, MessageWithFds};
use rustix::fd::AsFd;
use serde::Serialize;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut};
use std::num::NonZeroUsize;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use tokio::io::Interest;
use tokio::net::UnixStream as TokioUnixStream;
use tracing::{debug, trace};

/// Default maximum number of file descriptors per sendmsg() call.
///
/// Platform limits for SCM_RIGHTS vary (e.g., ~253 on Linux, ~512 on macOS).
/// We start with an optimistic value; if sendmsg() fails with EINVAL, the
/// batch size is automatically reduced and the send is retried.
pub const DEFAULT_MAX_FDS_PER_SENDMSG: NonZeroUsize = NonZeroUsize::new(500).unwrap();

/// Maximum FDs to expect in a single recvmsg() call.
/// Must be at least as large as the largest platform limit (~512 on macOS).
const MAX_FDS_PER_RECVMSG: usize = 512;

/// Read buffer size for incoming data.
const READ_BUFFER_SIZE: usize = 4096;

pub struct UnixSocketTransport {
    stream: TokioUnixStream,
}

impl UnixSocketTransport {
    pub fn new(stream: TokioUnixStream) -> Self {
        Self { stream }
    }

    pub fn split(self) -> (Sender, Receiver) {
        let stream = Arc::new(self.stream);

        (
            Sender {
                stream: Arc::clone(&stream),
                pretty: false,
                max_fds_per_sendmsg: DEFAULT_MAX_FDS_PER_SENDMSG,
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
    /// Maximum FDs to send per sendmsg() call. Configurable for testing.
    max_fds_per_sendmsg: NonZeroUsize,
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

    /// Set the maximum number of file descriptors to send per sendmsg() call.
    ///
    /// This is primarily useful for testing FD batching behavior. The default
    /// value ([`DEFAULT_MAX_FDS_PER_SENDMSG`]) is optimistic and may exceed
    /// some platform limits; if sendmsg() returns `EINVAL`, the batch size is
    /// automatically reduced and the send is retried.
    pub fn set_max_fds_per_sendmsg(&mut self, max_fds: NonZeroUsize) {
        self.max_fds_per_sendmsg = max_fds;
    }

    /// Send a notification without file descriptors.
    ///
    /// This is a convenience method that serializes the params and constructs
    /// the notification message automatically.
    pub async fn notify<P: Serialize>(&mut self, method: &str, params: P) -> Result<()> {
        self.notify_with_fds(method, params, Vec::new()).await
    }

    /// Send a notification with file descriptors.
    ///
    /// This is a convenience method that serializes the params and constructs
    /// the notification message automatically.
    pub async fn notify_with_fds<P: Serialize>(
        &mut self,
        method: &str,
        params: P,
        fds: Vec<OwnedFd>,
    ) -> Result<()> {
        let params_value = serde_json::to_value(params)?;
        let params_opt = if params_value.is_null() {
            None
        } else {
            Some(params_value)
        };
        let notification = JsonRpcNotification::new(method.to_string(), params_opt);
        let message = JsonRpcMessage::Notification(notification);
        let message_with_fds = MessageWithFds::new(message, fds);
        self.send(message_with_fds).await
    }

    pub async fn send(&mut self, message_with_fds: MessageWithFds) -> Result<()> {
        let serialized = if self.pretty {
            message_with_fds.serialize_pretty()?
        } else {
            message_with_fds.serialize()?
        };
        let data = serialized.into_bytes();

        trace!(
            "Sending message: {} with {} FDs",
            String::from_utf8_lossy(&data).trim(),
            message_with_fds.file_descriptors.len()
        );

        let fds = message_with_fds.file_descriptors;

        // Track how many bytes and FDs we've sent so far
        let mut bytes_sent = 0usize;
        let mut fds_sent = 0usize;

        // Current max FDs per batch - may be reduced if we hit EINVAL
        let mut current_max_fds = self.max_fds_per_sendmsg.get();

        // Send data with FDs in batches. Each sendmsg can only handle a limited number of FDs.
        // We send FDs with the data chunks, and any remaining FDs after all data is sent.
        while bytes_sent < data.len() || fds_sent < fds.len() {
            let remaining_data = &data[bytes_sent..];
            let remaining_fds = &fds[fds_sent..];

            // Determine how many FDs to send in this batch (up to current_max_fds)
            let fds_batch = remaining_fds.get(..current_max_fds).unwrap_or(remaining_fds);

            let result = self
                .stream
                .async_io(Interest::WRITABLE, || {
                    let sockfd = self.stream.as_fd();

                    if !fds_batch.is_empty() {
                        // Send with FDs using sendmsg with ancillary data
                        let borrowed_fds: Vec<_> = fds_batch.iter().map(|fd| fd.as_fd()).collect();

                        let mut buffer: Vec<u8> =
                            vec![0u8; rustix::cmsg_space!(ScmRights(MAX_FDS_PER_RECVMSG))];
                        let mut control = SendAncillaryBuffer::new(buffer.as_mut_slice());

                        if !control.push(SendAncillaryMessage::ScmRights(&borrowed_fds)) {
                            return Err(io::Error::other(
                                "Failed to add file descriptors to control message",
                            ));
                        }

                        // If we have data to send, include it; otherwise send a minimal byte
                        // (some systems require non-empty iov for ancillary data)
                        let iov = if !remaining_data.is_empty() {
                            [IoSlice::new(remaining_data)]
                        } else {
                            // Send a space byte that will be ignored by the receiver's JSON parser.
                            // RFC 8259 defines space (0x20) as insignificant whitespace, and
                            // serde_json's StreamDeserializer skips whitespace between values.
                            [IoSlice::new(b" ")]
                        };

                        rustix::net::sendmsg(sockfd, &iov, &mut control, SendFlags::empty())
                            .map_err(|e| to_io_error(e, "sendmsg"))
                    } else if !remaining_data.is_empty() {
                        // No FDs left, just send remaining data
                        rustix::net::send(sockfd, remaining_data, SendFlags::empty())
                            .map_err(|e| to_io_error(e, "send"))
                    } else {
                        // Nothing left to send
                        Ok(0)
                    }
                })
                .await;

            match result {
                Ok(sent) => {
                    // Update bytes sent (but only count actual data bytes, not padding)
                    if !remaining_data.is_empty() {
                        bytes_sent += sent;
                    }

                    // Update FDs sent
                    if !fds_batch.is_empty() {
                        fds_sent += fds_batch.len();
                        trace!(
                            "Sent {} FDs (total: {}/{}) with {} bytes",
                            fds_batch.len(),
                            fds_sent,
                            fds.len(),
                            sent
                        );
                    }

                    trace!(
                        "Progress: {}/{} bytes, {}/{} FDs",
                        bytes_sent,
                        data.len(),
                        fds_sent,
                        fds.len()
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::InvalidInput && fds_batch.len() > 1 => {
                    // EINVAL with multiple FDs likely means we exceeded the kernel's
                    // SCM_MAX_FD limit. Reduce batch size and retry.
                    let new_max = fds_batch.len() / 2;
                    debug!(
                        "sendmsg returned EINVAL with {} FDs, reducing batch size to {}",
                        fds_batch.len(),
                        new_max
                    );
                    current_max_fds = new_max;
                    // Don't update bytes_sent or fds_sent - we'll retry this batch
                    continue;
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }

        // If we discovered a lower limit, remember it for future sends
        if current_max_fds < self.max_fds_per_sendmsg.get() {
            debug!(
                "Learned kernel FD limit: reducing max_fds_per_sendmsg from {} to {}",
                self.max_fds_per_sendmsg, current_max_fds
            );
            // current_max_fds is at least 1 (we only reduce when fds_this_batch > 1)
            self.max_fds_per_sendmsg =
                NonZeroUsize::new(current_max_fds).expect("current_max_fds should be >= 1");
        }

        Ok(())
    }
}

pub struct Receiver {
    stream: Arc<TokioUnixStream>,
    buffer: Vec<u8>,
    fd_queue: VecDeque<OwnedFd>,
}

impl Receiver {
    /// Receive a message, returning an error on connection close.
    ///
    /// See also [`receive_opt`](Self::receive_opt) which returns `Ok(None)`
    /// on connection close instead of an error.
    pub async fn receive(&mut self) -> Result<MessageWithFds> {
        loop {
            if let Some(message) = self.try_parse_message()? {
                return Ok(message);
            }

            self.read_more_data().await?;
        }
    }

    /// Receive a message, returning `Ok(None)` on connection close.
    ///
    /// This is a convenience method that converts `Error::ConnectionClosed`
    /// to `Ok(None)`, which is useful for receiver loops:
    ///
    /// ```ignore
    /// while let Some(msg) = receiver.receive_opt().await? {
    ///     // handle message
    /// }
    /// ```
    ///
    /// See also [`receive`](Self::receive) which returns an error on
    /// connection close.
    pub async fn receive_opt(&mut self) -> Result<Option<MessageWithFds>> {
        match self.receive().await {
            Ok(msg) => Ok(Some(msg)),
            Err(Error::ConnectionClosed) => Ok(None),
            Err(e) => Err(e),
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

                // Read the fds count from the message and extract FDs
                let fd_count = get_fd_count(&value);

                if fd_count > self.fd_queue.len() {
                    return Err(Error::MismatchedCount {
                        expected: fd_count,
                        found: self.fd_queue.len(),
                    });
                }

                let fds: Vec<OwnedFd> = (0..fd_count)
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
                    vec![0u8; rustix::cmsg_space!(ScmRights(MAX_FDS_PER_RECVMSG))];
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
