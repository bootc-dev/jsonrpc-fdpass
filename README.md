# JSON-RPC 2.0 with Unix File Descriptor Passing

This repository contains both a protocol specification and a Rust implementation
(`jsonrpc-fdpass` crate) for JSON-RPC 2.0 with file descriptor passing over Unix
domain sockets.

## 1. Overview

This document specifies a variant of the JSON-RPC 2.0 protocol designed for reliable inter-process communication (IPC) over stream-oriented sockets. It is intended for use on POSIX-compliant systems where SOCK_SEQPACKET is unavailable (such as macOS) or undesirable.

It uses Unix domain sockets of type SOCK_STREAM, leverages JSON's self-delimiting nature for message framing, and extends the JSON-RPC 2.0 data model to support passing file descriptors using ancillary data.

The primary design goal is to provide a portable, unambiguous protocol for passing file descriptors alongside structured JSON messages over a standard byte stream.

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in RFC 2119.

## 2. Transport and Framing

### 2.1. Socket Type

The transport for this protocol MUST be a Unix domain socket created with the type SOCK_STREAM.

### 2.2. Message Framing

JSON is a self-delimiting format—a compliant parser can determine where one JSON value ends and the next begins without external delimiters. This protocol leverages streaming JSON parsing for message framing.

* The JSON text MUST be encoded using UTF-8.
* Each message MUST be a complete, valid JSON object.
* Whitespace between messages is permitted but not required.

### 2.3. Transmission Rules

To ensure file descriptors are correctly associated with their corresponding messages, a sending party MUST adhere to the following rules:

1. **File Descriptor Ordering:** All file descriptors referenced by a message MUST be sent (via ancillary data) before or with the final bytes of that message. The receiver dequeues FDs in order as complete messages are parsed; if the required FDs have not yet arrived, the connection is terminated with a Mismatched Count error.

## 3. Message Format

### 3.1. Base Protocol

The protocol is a strict extension of JSON-RPC 2.0. All standard rules regarding the structure of Request, Response, and Notification objects apply.

### 3.2. File Descriptor Count Field

When a JSON-RPC message is accompanied by file descriptors, the message MUST include an `fds` field at the top level of the JSON object. This field indicates how many file descriptors are attached to the message.

```json
{
  "jsonrpc": "2.0",
  "method": "writeFile",
  "params": { "data": "..." },
  "id": 1,
  "fds": 1
}
```

* `fds` (integer): A non-negative integer specifying the number of file descriptors attached to this message.

When N file descriptors are passed with a message (N > 0), the `fds` field MUST be present and set to N. The file descriptors are passed positionally—the application layer defines the semantic mapping between FD positions and parameters. If `fds` is 0 or absent, no file descriptors are associated with the message.

## 4. File Descriptor Passing Mechanism

File descriptors MUST be passed using ancillary data via the sendmsg(2) and recvmsg(2) system calls.

* The control message header (cmsghdr) MUST specify cmsg_level as SOL_SOCKET and cmsg_type as SCM_RIGHTS.
* The control message data (CMSG_DATA) MUST contain the array of integer file descriptors.

## 5. Receiver Logic

Because SOCK_STREAM does not preserve message boundaries, the receiver MUST implement its own buffering and parsing logic. The logic MUST correctly associate file descriptors with their corresponding message by processing both the byte stream and the ancillary data stream in the strict order they are received.

1. **State Maintenance:** The receiver MUST maintain two data structures in its state:
   * A byte buffer for incoming data from the socket.
   * A **first-in, first-out (FIFO) queue** for received file descriptors.

2. **Reading:** When the recvmsg(2) system call returns data, any received bytes MUST be appended to the end of the byte buffer. Any received file descriptors MUST be enqueued, in the order they were provided by the system call, to the back of the file descriptor queue.

3. **Processing Loop:** The receiver MUST process the byte buffer by repeatedly performing the following steps until no more complete messages can be extracted:
   1. **Streaming Parse:** Attempt to parse a complete JSON object from the beginning of the byte buffer using a streaming JSON parser. If the buffer contains an incomplete JSON value (e.g., the parser encounters EOF mid-value), the processing loop terminates until more data is received.
   2. **Handle Parse Result:** If parsing succeeds, record the number of bytes consumed. If parsing fails with a syntax error (not EOF), this is a fatal Framing Error (see Section 7), and the connection MUST be closed.
   3. **Read FD Count:** Read the `fds` field from the parsed JSON message to determine the number of file descriptors (N) associated with this message. If the field is absent, N is 0.
   4. **Check FD Queue:** Check if the file descriptor queue contains at least N FDs. If it contains fewer than N FDs, this is a fatal Mismatched Count error (see Section 7). The protocol state is desynchronized, and the connection MUST be closed.
   5. **Dequeue and Associate:** Dequeue the first N file descriptors from the front of the queue. These FDs correspond positionally (0 through N-1) to the file descriptors expected by the application for this message.
   6. **Dispatch:** The fully-formed message (with FDs) is now ready and SHOULD be dispatched to the application logic for handling.
   7. **Consume Bytes:** The consumed bytes MUST be removed from the front of the byte buffer.

This algorithmic approach ensures that file descriptors are always correctly matched to their corresponding messages, even when multiple messages are received in a single recvmsg() call.

## 6. Examples

### 6.1. Request with a Single File Descriptor

A client asks a server to write to a file.

**Client-side Action:**

1. Open a file, yielding fd = 5.
2. Construct the JSON payload:
   ```json
   {"jsonrpc":"2.0","method":"writeFile","params":{"data":"..."},"id":1,"fds":1}
   ```
3. Call sendmsg() with the JSON payload and one control message containing the file descriptor 5.

**Server-side Action:**

1. Call recvmsg(), receiving a data chunk and the file descriptor 5.
2. Append the data to its byte buffer. Enqueue 5 into its FD queue.
3. Begin the processing loop. The streaming parser finds a complete JSON object.
4. It parses the JSON message. It reads `fds: 1`, so N=1.
5. It checks that the FD queue size is >= 1. It is.
6. It dequeues the FD 5 and associates it with the message.
7. The complete message is dispatched. The processed bytes are removed from the buffer.

## 7. Error Handling

Protocol errors related to framing and file descriptor handling are fatal, as they indicate a desynchronization between the sender and receiver. Upon detecting such an error, the receiver MUST close the connection.

The primary error code for these issues is:

| Code    | Message                | Meaning                                                                                     |
|---------|------------------------|---------------------------------------------------------------------------------------------|
| -32050  | File Descriptor Error  | A fatal error occurred during protocol framing or FD association. The connection state is now invalid. |

**Conditions that MUST be treated as fatal errors:**

* **Framing Error:** The byte stream cannot be parsed as valid JSON (syntax error, not incomplete data).
* **Mismatched Count:** A parsed message's `fds` field specifies N file descriptors, but the receiver's file descriptor queue contains fewer than N available FDs at the time of processing.

## 8. Security Considerations

The security considerations are identical to those for other Unix domain socket protocols:

* **Socket Permissions:** Filesystem permissions on the socket file are the primary access control mechanism.
* **Trust Boundary:** The communicating processes must have a degree of mutual trust, as passing a file descriptor is a grant of capability.
* **Resource Management:** The receiving process is responsible for closing all file descriptors it receives to prevent resource leaks. If a connection is terminated due to a protocol error, the receiver MUST ensure that any FDs remaining in its queue are closed.
