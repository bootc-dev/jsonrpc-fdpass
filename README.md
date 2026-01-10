# JSON-RPC 2.0 with Unix File Descriptor Passing

This repository contains both a protocol specification and a Rust implementation
(`jsonrpc-fdpass` crate) for JSON-RPC 2.0 with file descriptor passing over Unix
domain sockets.

## 1. Overview

This document specifies a variant of the JSON-RPC 2.0 protocol designed for reliable inter-process communication (IPC) over stream-oriented sockets. It is intended for use on POSIX-compliant systems where SOCK_SEQPACKET is unavailable (such as macOS) or undesirable.

It uses Unix domain sockets of type SOCK_STREAM, mandates Newline Delimited JSON (NDJSON) for message framing, and extends the JSON-RPC 2.0 data model to support passing file descriptors using ancillary data.

The primary design goal is to provide a portable, unambiguous protocol for passing file descriptors alongside structured JSON messages over a standard byte stream.

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in RFC 2119.

## 2. Transport and Framing

### 2.1. Socket Type

The transport for this protocol MUST be a Unix domain socket created with the type SOCK_STREAM.

### 2.2. Message Framing

All JSON-RPC messages MUST be framed using the Newline Delimited JSON (NDJSON) format.

* Each JSON message MUST be a single line of text.
* The JSON text MUST be encoded using UTF-8.
* Each line MUST be terminated by a single newline character (`\n`, ASCII 0x0A).
* Carriage returns (`\r`) MUST NOT be used.

### 2.3. Transmission Rule

To unambiguously associate file descriptors with their corresponding message, a sending party **MUST** adhere to the following rule:

**A single sendmsg(2) system call MUST contain exactly one and only one complete NDJSON message** (the JSON text and its terminating newline character).

File descriptors intended for that message MUST be included as ancillary data in that same sendmsg() call. A sendmsg() call that includes file descriptors MUST also contain a complete NDJSON message.

* **Rationale:** This strict 1:1 mapping between a sendmsg() call, a single NDJSON message, and its associated file descriptors is the core of the protocol. It leverages the kernel's guarantee that data and ancillary data from a single sendmsg call are delivered atomically to the underlying transport. This allows the receiver to reliably associate FDs with messages even if multiple messages are coalesced in the stream.

## 3. Message Format

### 3.1. Base Protocol

The protocol is a strict extension of JSON-RPC 2.0. All standard rules regarding the structure of Request, Response, and Notification objects apply.

### 3.2. File Descriptor Placeholder

To represent a file descriptor within a JSON message, a **File Descriptor Placeholder** object MUST be used.

The placeholder object has the following structure:

```json
{
  "__jsonrpc_fd__": true,
  "index": <integer>
}
```

* `__jsonrpc_fd__` (boolean): A marker field that MUST be present and set to true.
* `index` (integer): A non-negative, zero-based integer.

When N file descriptors are passed with a message (N > 0), the JSON payload MUST contain exactly N placeholder objects. The index values in these placeholders MUST be unique and MUST form a complete, dense range from 0 to N-1.

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
   1. **Scan for Delimiter:** Scan the byte buffer for the first occurrence of a newline character (`\n`). If none is found, the processing loop terminates until more data is received.
   2. **Extract Message Bytes:** Extract the sequence of bytes from the start of the buffer up to and including the newline character.
   3. **Parse Message:** Attempt to parse the extracted bytes (excluding the newline) as a JSON object. If parsing fails, this is a fatal Framing Error (see Section 7), and the connection MUST be closed.
   4. **Count Placeholders:** Count the number of File Descriptor Placeholders (N) within the parsed JSON message.
   5. **Check FD Queue:** Check if the file descriptor queue contains at least N FDs. If it contains fewer than N FDs, this is a fatal Mismatched Count error (see Section 7). The protocol state is desynchronized, and the connection MUST be closed.
   6. **Dequeue and Associate:** Dequeue the first N file descriptors from the front of the queue. These FDs correspond, in order, to the placeholders with indices 0 through N-1. The receiver MUST substitute the placeholder objects in its internal representation of the message with these actual file descriptor values.
   7. **Dispatch:** The fully-formed message (with FDs) is now ready and SHOULD be dispatched to the application logic for handling.
   8. **Consume Bytes:** The extracted message bytes (including the newline) MUST be removed from the front of the byte buffer.

This algorithmic approach ensures that file descriptors are always correctly matched to their corresponding messages, even when multiple messages are received in a single recvmsg() call.

## 6. Examples

### 6.1. Request with a Single File Descriptor

A client asks a server to write to a file.

**Client-side Action:**

1. Open a file, yielding fd = 5.
2. Construct the JSON payload string:
   ```json
   {"jsonrpc":"2.0","method":"writeFile","params":{"file":{"__jsonrpc_fd__":true,"index":0},"data":"..."},"id":1}
   ```
3. Prepare the data for sending: payload_string + "\n"
4. Call sendmsg() with the final data buffer and one control message containing the file descriptor 5.

**Server-side Action:**

1. Call recvmsg(), receiving a data chunk and the file descriptor 5.
2. Append the data to its byte buffer. Enqueue 5 into its FD queue.
3. Begin the processing loop. It finds a newline.
4. It extracts and parses the JSON message. It counts N=1 placeholder.
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

* **Framing Error:** A received line of text (terminated by `\n`) cannot be parsed as valid JSON.
* **Mismatched Count:** A parsed message requires N file descriptors, but the receiver's file descriptor queue contains fewer than N available FDs at the time of processing.
* **Invalid Placeholders:** The index values in a message's placeholders are not unique or do not form a dense range 0..N-1.
* **Dangling File Descriptors:** A message contains zero placeholders, but file descriptors were received with it. This is implicitly handled by the receiver logic, but a sender that does this is non-compliant.

## 8. Security Considerations

The security considerations are identical to those for other Unix domain socket protocols:

* **Socket Permissions:** Filesystem permissions on the socket file are the primary access control mechanism.
* **Trust Boundary:** The communicating processes must have a degree of mutual trust, as passing a file descriptor is a grant of capability.
* **Resource Management:** The receiving process is responsible for closing all file descriptors it receives to prevent resource leaks. If a connection is terminated due to a protocol error, the receiver MUST ensure that any FDs remaining in its queue are closed.