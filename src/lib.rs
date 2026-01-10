//! # JSON-RPC 2.0 with Unix File Descriptor Passing
//!
//! This crate provides an implementation of JSON-RPC 2.0 with file descriptor passing over Unix
//! domain sockets. It enables reliable inter-process communication (IPC) with the ability to
//! pass file descriptors alongside JSON-RPC messages.
//!
//! ## Features
//!
//! - **JSON-RPC 2.0 compliance**: Full support for requests, responses, and notifications
//! - **File descriptor passing**: Pass file descriptors using Unix socket ancillary data
//! - **NDJSON framing**: Newline-delimited JSON for reliable message boundaries
//! - **Async support**: Built on tokio for high-performance async I/O
//! - **Type-safe**: Rust's type system ensures correct message handling
//!
//! ## Quick Start
//!
//! ### Server Example
//!
//! ```rust,no_run
//! use jsonrpc_fdpass::{Server, Result};
//! use std::fs::File;
//! use serde_json::Value;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let mut server = Server::new();
//!     
//!     server.register_method("read_file", |_method, _params, fds| {
//!         if let Some(fd) = fds.into_iter().next() {
//!             let mut file = File::from(fd);
//!             let mut contents = String::new();
//!             use std::io::Read;
//!             file.read_to_string(&mut contents).unwrap();
//!             Ok((Some(Value::String(contents)), Vec::new()))
//!         } else {
//!             Err(jsonrpc_fdpass::Error::InvalidMessage("No FD provided".into()))
//!         }
//!     });
//!     
//!     server.listen("/tmp/test.sock").await
//! }
//! ```
//!
//! ### Client Example
//!
//! ```rust,no_run
//! use jsonrpc_fdpass::{Client, Result};
//! use std::fs::File;
//! use std::os::unix::io::OwnedFd;
//! use serde_json::json;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let mut client = Client::connect("/tmp/test.sock").await?;
//!     
//!     let file = File::open("example.txt").unwrap();
//!     let fd: OwnedFd = file.into();
//!     
//!     let params = json!({
//!         "file": {
//!             "__jsonrpc_fd__": true,
//!             "index": 0
//!         }
//!     });
//!     
//!     client.call_method("read_file", Some(params), vec![fd]).await?;
//!     
//!     Ok(())
//! }
//! ```
//!
//! ## Protocol Details
//!
//! This implementation follows the NDJSON JSON-RPC with File Descriptor Passing specification:
//!
//! - Uses Unix domain sockets (SOCK_STREAM)
//! - Messages are framed using newline-delimited JSON (NDJSON)
//! - File descriptors are passed using ancillary data via sendmsg(2)/recvmsg(2)
//! - Each sendmsg() call contains exactly one complete NDJSON message
//! - File descriptors are represented in JSON using placeholder objects
//!
//! ### File Descriptor Placeholders
//!
//! File descriptors are represented in JSON messages using special placeholder objects:
//!
//! ```json
//! {
//!   "__jsonrpc_fd__": true,
//!   "index": 0
//! }
//! ```
//!
//! The `index` field corresponds to the position of the file descriptor in the ancillary data.

#![forbid(unsafe_code)]

pub mod client;
pub mod error;
pub mod message;
pub mod server;
pub mod transport;

pub use client::Client;
pub use error::{Error, Result};
pub use jsonrpsee::types::error::ErrorObject as JsonRpcError;
pub use message::{
    file_descriptor_error, FileDescriptorPlaceholder, JsonRpcMessage, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, MessageWithFds, FILE_DESCRIPTOR_ERROR_CODE,
};
pub use server::Server;
pub use transport::{Receiver, Sender, UnixSocketTransport};
