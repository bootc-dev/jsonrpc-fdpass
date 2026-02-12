# Related Work

This document describes related protocols and implementations for IPC with file
descriptor passing.

## systemd's Varlink with FD Passing

systemd has implemented file descriptor passing as an extension to the
[varlink](https://varlink.org/) protocol. This is relevant prior art for
jsonrpc-fdpass.

### Not Part of the Varlink Standard

The official varlink specification at varlink.org does **not** include fd
passing. The spec defines a simple JSON-based RPC protocol over Unix sockets
(or TCP), with messages terminated by NUL bytes. FD passing is a
**systemd-specific extension**.

This means other varlink implementations (Rust, Go, Python, etc.) do not
support fd passing out of the box.

### Public API

systemd provides fd passing through its `sd-varlink` API in libsystemd:

```c
// Push fds to send with the next message
int sd_varlink_push_fd(sd_varlink *v, int fd);      // Returns index (0, 1, 2...)
int sd_varlink_push_dup_fd(sd_varlink *v, int fd);  // Dups the fd first
int sd_varlink_reset_fds(sd_varlink *v);            // Clear pending fds

// Retrieve fds from the current message
int sd_varlink_peek_fd(sd_varlink *v, size_t i);    // Borrow fd at index
int sd_varlink_take_fd(sd_varlink *v, size_t i);    // Take ownership of fd
int sd_varlink_get_n_fds(sd_varlink *v);            // Count of fds

// Server flags to enable fd passing
SD_VARLINK_SERVER_ALLOW_FD_PASSING_INPUT   // Allow receiving fds
SD_VARLINK_SERVER_ALLOW_FD_PASSING_OUTPUT  // Allow sending fds
```

### Wire Protocol

systemd uses `SCM_RIGHTS` via `sendmsg()`/`recvmsg()` to pass file descriptors
alongside JSON message bytes. The implementation is in
`src/libsystemd/sd-varlink/sd-varlink.c`.

FDs are associated with messages using a queue structure (`VarlinkJsonQueueItem`)
that binds each JSON message to its accompanying fds, preserving message
boundaries.

### JSON Index References

Within JSON messages, fds are referenced by **index** rather than fd number:

```json
{
  "mountFileDescriptor": 0,
  "designator": "root",
  "writable": true
}
```

The sender calls `sd_varlink_push_fd()` which returns an index (0, 1, 2, ...),
and that index is included in the JSON. The receiver uses
`sd_varlink_take_fd(link, 0)` to retrieve the actual fd.

### Production Usage

systemd uses varlink fd passing in several services:

| Service | Use Case |
|---------|----------|
| **mountfsd** | Passing `fsmount` fds for disk image mounts |
| **nsresourced** | Passing TAP interface fds for network namespaces |
| **machined** | Passing PTY fds for container shell access |
| **networkd** | Network configuration fds |
| **logind** | Session-related fds |
| **bootctl** | Boot loader installation |

### Comparison with jsonrpc-fdpass

| Aspect | systemd varlink | jsonrpc-fdpass |
|--------|-----------------|----------------|
| FD count field | Index references in params | Top-level `fds` count |
| Base protocol | varlink (custom) | JSON-RPC 2.0 |
| Specification | Extension to existing protocol | Standalone spec |
| Implementation | C (libsystemd) | Rust |
| Batching | Not specified | Explicit whitespace batching |

Key differences:

1. **FD indication**: jsonrpc-fdpass uses a top-level `"fds": N` field to
   indicate how many fds accompany a message. systemd's approach embeds fd
   indices directly in parameters (e.g., `"mountFileDescriptor": 0`).

2. **Batching**: jsonrpc-fdpass explicitly specifies how to batch fds across
   multiple `sendmsg()` calls using whitespace bytes. systemd doesn't appear
   to have a formal batching mechanism in its spec.

3. **Language support**: jsonrpc-fdpass provides a Rust implementation;
   systemd's is C-only and tightly integrated with libsystemd.

## containerd ttrpc

[ttrpc](https://github.com/containerd/ttrpc) is containerd's lightweight RPC
protocol, an alternative to gRPC for local IPC. There has been work to add fd
passing support:

- [containerd/ttrpc#75](https://github.com/containerd/ttrpc/pull/75) - PR
  exploring fd passing

ttrpc uses protobuf for message encoding rather than JSON.

## D-Bus

D-Bus supports fd passing via `UNIX_FD` type in its type system. This is a
well-established protocol but is more complex than JSON-RPC or varlink, with
its own binary wire format and elaborate type system.

## See Also

- [varlink.org](https://varlink.org/) - Official varlink specification
- [systemd sd-varlink](https://www.freedesktop.org/software/systemd/man/latest/sd-varlink.html) - systemd's varlink API
- Discussion that motivated this research: [containers/buildah#6675](https://github.com/containers/buildah/pull/6675)
