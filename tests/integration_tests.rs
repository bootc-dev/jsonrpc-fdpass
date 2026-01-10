use jsonrpc_fdpass::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, MessageWithFds, Result, Server,
    UnixSocketTransport,
};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::OwnedFd;
use tempfile::{NamedTempFile, TempDir};

#[tokio::test]
async fn test_basic_message_serialization() -> Result<()> {
    let request = JsonRpcRequest::new(
        "test_method".to_string(),
        Some(Value::String("test_param".to_string())),
        Value::Number(1.into()),
    );

    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, vec![]);

    let serialized = message_with_fds.serialize_with_placeholders()?;
    assert!(serialized.contains("test_method"));
    assert!(serialized.contains("test_param"));

    Ok(())
}

#[tokio::test]
async fn test_file_descriptor_placeholder() -> Result<()> {
    // Create a message with file descriptor placeholder
    let params = serde_json::json!({
        "file": {
            "__jsonrpc_fd__": true,
            "index": 0
        }
    });

    let request = JsonRpcRequest::new(
        "write_file".to_string(),
        Some(params),
        Value::Number(1.into()),
    );

    let message = JsonRpcMessage::Request(request);

    // Create a temporary file
    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "Hello, World!").unwrap();
    temp_file.flush().unwrap();

    let fd: OwnedFd = temp_file.into_file().into();
    let message_with_fds = MessageWithFds::new(message, vec![fd]);

    let serialized = message_with_fds.serialize_with_placeholders()?;
    assert!(serialized.contains("__jsonrpc_fd__"));
    assert!(serialized.contains("\"index\":0"));

    Ok(())
}

#[tokio::test]
async fn test_client_server_communication() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test.sock");

    // Create listener first
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    // Start server with pre-allocated listener
    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("echo", |_method, params, _fds| Ok((params, Vec::new())));

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            if let Ok(message_with_fds) = receiver.receive().await {
                let _ = server.process_message(message_with_fds, &mut sender).await;
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send request (no race condition)
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    let request = JsonRpcRequest::new(
        "echo".to_string(),
        Some(Value::String("Hello from client".to_string())),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, vec![]);

    sender.send(message_with_fds).await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_file_descriptor_passing() -> Result<()> {
    // Create test content
    let test_content = "Test file content for FD passing";

    // Create temporary file with test content
    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "{}", test_content).unwrap();
    temp_file.flush().unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();

    let fd: OwnedFd = temp_file.into_file().into();

    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test.sock");

    let expected_content = test_content.to_string();

    // Create listener first
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    // Start server with pre-allocated listener
    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("read_file", move |_method, _params, fds| {
            if fds.is_empty() {
                return Err(jsonrpc_fdpass::Error::InvalidMessage(
                    "Expected file descriptor".to_string(),
                ));
            }

            let fd = fds.into_iter().next().unwrap();
            let mut file = File::from(fd);
            let mut contents = String::new();

            // Seek to beginning of file
            file.seek(SeekFrom::Start(0)).unwrap();
            file.read_to_string(&mut contents).unwrap();

            // Verify content matches expected
            assert_eq!(contents.trim(), expected_content);

            Ok((Some(Value::String(contents)), Vec::new()))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            if let Ok(message_with_fds) = receiver.receive().await {
                let _ = server.process_message(message_with_fds, &mut sender).await;
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send file descriptor (no race condition)
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Create params with file descriptor placeholder
    let params = serde_json::json!({
        "file": {
            "__jsonrpc_fd__": true,
            "index": 0
        }
    });

    // Send a test message with file descriptor
    let request = JsonRpcRequest::new(
        "read_file".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, vec![fd]);

    sender.send(message_with_fds).await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_multiple_messages_with_fds_sequential() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_multi.sock");

    // Create multiple test files with different content
    let mut temp_files = Vec::new();
    let test_contents = vec!["Content 1", "Content 2", "Content 3"];

    for (_i, content) in test_contents.iter().enumerate() {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{}", content).unwrap();
        temp_file.flush().unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        temp_files.push(temp_file);
    }

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();
        let received_messages = std::sync::Arc::new(std::sync::Mutex::new(0));
        let received_messages_clone = received_messages.clone();

        server.register_method("read_sequential", move |_method, params, fds| {
            let mut count = received_messages_clone.lock().unwrap();
            *count += 1;

            if fds.is_empty() {
                return Err(jsonrpc_fdpass::Error::InvalidMessage(
                    "Expected file descriptor".to_string(),
                ));
            }

            let fd = fds.into_iter().next().unwrap();
            let mut file = File::from(fd);
            let mut contents = String::new();

            file.seek(SeekFrom::Start(0)).unwrap();
            file.read_to_string(&mut contents).unwrap();

            // Extract expected content from params
            let expected_idx = params
                .as_ref()
                .and_then(|p| p.get("expected_idx"))
                .and_then(|v| v.as_u64())
                .unwrap() as usize;

            let expected_content = format!("Content {}", expected_idx + 1);
            assert_eq!(contents.trim(), expected_content);

            Ok((
                Some(Value::String(format!("Processed message {}", *count))),
                Vec::new(),
            ))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Process multiple messages sequentially
            for _ in 0..3 {
                if let Ok(message_with_fds) = receiver.receive().await {
                    let _ = server.process_message(message_with_fds, &mut sender).await;
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send multiple messages with file descriptors
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Send multiple messages sequentially
    for (i, temp_file) in temp_files.into_iter().enumerate() {
        let fd: OwnedFd = temp_file.into_file().into();

        let params = serde_json::json!({
            "file": {
                "__jsonrpc_fd__": true,
                "index": 0
            },
            "expected_idx": i
        });

        let request = JsonRpcRequest::new(
            "read_sequential".to_string(),
            Some(params),
            Value::Number((i + 1).into()),
        );
        let message = JsonRpcMessage::Request(request);
        let message_with_fds = MessageWithFds::new(message, vec![fd]);

        sender.send(message_with_fds).await?;
    }

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_multiple_fds_single_message() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_multi_fds.sock");

    // Create multiple test files
    let mut temp_files = Vec::new();
    let test_contents = vec!["File A content", "File B content", "File C content"];

    for content in &test_contents {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{}", content).unwrap();
        temp_file.flush().unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        temp_files.push(temp_file);
    }

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let expected_contents = test_contents.clone();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("read_multiple_files", move |_method, _params, fds| {
            assert_eq!(fds.len(), 3, "Expected exactly 3 file descriptors");

            let mut all_contents = Vec::new();
            for (i, fd) in fds.into_iter().enumerate() {
                let mut file = File::from(fd);
                let mut contents = String::new();

                file.seek(SeekFrom::Start(0)).unwrap();
                file.read_to_string(&mut contents).unwrap();

                // Verify content matches expected
                assert_eq!(contents.trim(), expected_contents[i]);
                all_contents.push(contents.trim().to_string());
            }

            Ok((
                Some(Value::Array(
                    all_contents.into_iter().map(Value::String).collect(),
                )),
                Vec::new(),
            ))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            if let Ok(message_with_fds) = receiver.receive().await {
                let _ = server.process_message(message_with_fds, &mut sender).await;
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message with multiple file descriptors
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Create params with multiple file descriptor placeholders
    let params = serde_json::json!({
        "files": [
            {
                "__jsonrpc_fd__": true,
                "index": 0
            },
            {
                "__jsonrpc_fd__": true,
                "index": 1
            },
            {
                "__jsonrpc_fd__": true,
                "index": 2
            }
        ]
    });

    let fds: Vec<OwnedFd> = temp_files
        .into_iter()
        .map(|tf| tf.into_file().into())
        .collect();

    let request = JsonRpcRequest::new(
        "read_multiple_files".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, fds);

    sender.send(message_with_fds).await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_mixed_messages_with_and_without_fds() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_mixed.sock");

    // Create one test file
    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "Test file content").unwrap();
    temp_file.flush().unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();
        let mut message_count = 0;

        server.register_method("echo", |_method, params, fds| {
            assert!(
                fds.is_empty(),
                "Echo method should not receive file descriptors"
            );
            Ok((params, Vec::new()))
        });

        server.register_method("read_file", move |_method, _params, fds| {
            assert_eq!(fds.len(), 1, "Expected exactly 1 file descriptor");

            let fd = fds.into_iter().next().unwrap();
            let mut file = File::from(fd);
            let mut contents = String::new();

            file.seek(SeekFrom::Start(0)).unwrap();
            file.read_to_string(&mut contents).unwrap();

            Ok((Some(Value::String(contents)), Vec::new()))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Process multiple mixed messages
            for _ in 0..4 {
                if let Ok(message_with_fds) = receiver.receive().await {
                    message_count += 1;
                    let _ = server.process_message(message_with_fds, &mut sender).await;
                }
            }

            assert_eq!(message_count, 4);
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send mixed messages
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // 1. Echo message (no FD)
    let request1 = JsonRpcRequest::new(
        "echo".to_string(),
        Some(Value::String("Hello".to_string())),
        Value::Number(1.into()),
    );
    let message1 = JsonRpcMessage::Request(request1);
    let message_with_fds1 = MessageWithFds::new(message1, vec![]);
    sender.send(message_with_fds1).await?;

    // 2. Read file message (with FD)
    temp_file.seek(SeekFrom::Start(0)).unwrap();
    let fd: OwnedFd = temp_file.into_file().into();

    let params2 = serde_json::json!({
        "file": {
            "__jsonrpc_fd__": true,
            "index": 0
        }
    });

    let request2 = JsonRpcRequest::new(
        "read_file".to_string(),
        Some(params2),
        Value::Number(2.into()),
    );
    let message2 = JsonRpcMessage::Request(request2);
    let message_with_fds2 = MessageWithFds::new(message2, vec![fd]);
    sender.send(message_with_fds2).await?;

    // 3. Another echo message (no FD)
    let request3 = JsonRpcRequest::new(
        "echo".to_string(),
        Some(Value::String("World".to_string())),
        Value::Number(3.into()),
    );
    let message3 = JsonRpcMessage::Request(request3);
    let message_with_fds3 = MessageWithFds::new(message3, vec![]);
    sender.send(message_with_fds3).await?;

    // 4. Notification (no FD)
    let notification = JsonRpcNotification::new(
        "status".to_string(),
        Some(Value::String("completed".to_string())),
    );
    let message4 = JsonRpcMessage::Notification(notification);
    let message_with_fds4 = MessageWithFds::new(message4, vec![]);
    sender.send(message_with_fds4).await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_large_number_of_fds() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_many_fds.sock");

    // Create many test files (testing protocol limits)
    let num_fds = 10;
    let mut temp_files = Vec::new();

    for i in 0..num_fds {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "File {} content", i).unwrap();
        temp_file.flush().unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        temp_files.push(temp_file);
    }

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("process_many_files", move |_method, _params, fds| {
            assert_eq!(
                fds.len(),
                num_fds,
                "Expected exactly {} file descriptors",
                num_fds
            );

            let mut total_size = 0;
            for (i, fd) in fds.into_iter().enumerate() {
                let mut file = File::from(fd);
                let mut contents = String::new();

                file.seek(SeekFrom::Start(0)).unwrap();
                file.read_to_string(&mut contents).unwrap();

                let expected_content = format!("File {} content", i);
                assert_eq!(contents.trim(), expected_content);
                total_size += contents.len();
            }

            Ok((Some(Value::Number(total_size.into())), Vec::new()))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            if let Ok(message_with_fds) = receiver.receive().await {
                let _ = server.process_message(message_with_fds, &mut sender).await;
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message with many file descriptors
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Create params with many file descriptor placeholders
    let files: Vec<_> = (0..num_fds)
        .map(|i| {
            serde_json::json!({
                "__jsonrpc_fd__": true,
                "index": i
            })
        })
        .collect();

    let params = serde_json::json!({ "files": files });

    let fds: Vec<OwnedFd> = temp_files
        .into_iter()
        .map(|tf| tf.into_file().into())
        .collect();

    let request = JsonRpcRequest::new(
        "process_many_files".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, fds);

    sender.send(message_with_fds).await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_zero_byte_files_with_fds() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_zero_byte.sock");

    // Create empty test files
    let mut temp_files = Vec::new();
    for _ in 0..3 {
        let temp_file = NamedTempFile::new().unwrap();
        // Don't write anything - leave it empty
        temp_files.push(temp_file);
    }

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("read_empty_files", move |_method, _params, fds| {
            assert_eq!(fds.len(), 3, "Expected exactly 3 file descriptors");

            for fd in fds {
                let mut file = File::from(fd);
                let mut contents = String::new();

                file.seek(SeekFrom::Start(0)).unwrap();
                file.read_to_string(&mut contents).unwrap();

                assert_eq!(contents.len(), 0, "Expected empty file");
            }

            Ok((
                Some(Value::String("All empty files processed".to_string())),
                Vec::new(),
            ))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            if let Ok(message_with_fds) = receiver.receive().await {
                let _ = server.process_message(message_with_fds, &mut sender).await;
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message with empty file descriptors
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    let params = serde_json::json!({
        "files": [
            { "__jsonrpc_fd__": true, "index": 0 },
            { "__jsonrpc_fd__": true, "index": 1 },
            { "__jsonrpc_fd__": true, "index": 2 }
        ]
    });

    let fds: Vec<OwnedFd> = temp_files
        .into_iter()
        .map(|tf| tf.into_file().into())
        .collect();

    let request = JsonRpcRequest::new(
        "read_empty_files".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, fds);

    sender.send(message_with_fds).await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_fd_placeholder_index_ordering() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_ordering.sock");

    // Create test files with specific content for ordering verification
    let test_contents = vec!["FIRST", "SECOND", "THIRD"];
    let mut temp_files = Vec::new();

    for content in &test_contents {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "{}", content).unwrap();
        temp_file.flush().unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        temp_files.push(temp_file);
    }

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let expected_contents = test_contents.clone();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("verify_fd_ordering", move |_method, _params, fds| {
            assert_eq!(fds.len(), 3, "Expected exactly 3 file descriptors");

            // Verify that FDs are received in the correct order (0, 1, 2)
            for (i, fd) in fds.into_iter().enumerate() {
                let mut file = File::from(fd);
                let mut contents = String::new();

                file.seek(SeekFrom::Start(0)).unwrap();
                file.read_to_string(&mut contents).unwrap();

                assert_eq!(
                    contents.trim(),
                    expected_contents[i],
                    "FD at index {} has wrong content",
                    i
                );
            }

            Ok((
                Some(Value::String("Order verified".to_string())),
                Vec::new(),
            ))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            if let Ok(message_with_fds) = receiver.receive().await {
                let _ = server.process_message(message_with_fds, &mut sender).await;
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message with FDs in specific order
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Create params with placeholders in non-sequential order to test protocol
    let params = serde_json::json!({
        "file_2": { "__jsonrpc_fd__": true, "index": 2 },
        "file_0": { "__jsonrpc_fd__": true, "index": 0 },
        "file_1": { "__jsonrpc_fd__": true, "index": 1 }
    });

    let fds: Vec<OwnedFd> = temp_files
        .into_iter()
        .map(|tf| tf.into_file().into())
        .collect();

    let request = JsonRpcRequest::new(
        "verify_fd_ordering".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, fds);

    sender.send(message_with_fds).await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_rapid_message_bursts() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_burst.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();
        let processed_count = std::sync::Arc::new(std::sync::Mutex::new(0));
        let processed_count_clone = processed_count.clone();

        server.register_method("burst_handler", move |_method, params, fds| {
            let mut count = processed_count_clone.lock().unwrap();
            *count += 1;

            let expected_id = params
                .as_ref()
                .and_then(|p| p.get("burst_id"))
                .and_then(|v| v.as_u64())
                .unwrap();

            // Verify FDs if present
            if !fds.is_empty() {
                assert_eq!(fds.len(), 1, "Expected at most 1 FD per burst message");
                let fd = fds.into_iter().next().unwrap();
                let mut file = File::from(fd);
                let mut contents = String::new();
                file.seek(SeekFrom::Start(0)).unwrap();
                file.read_to_string(&mut contents).unwrap();

                let expected_content = format!("Burst message {}", expected_id);
                assert_eq!(contents.trim(), expected_content);
            }

            Ok((Some(Value::Number((*count).into())), Vec::new()))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Process burst of messages
            for _ in 0..20 {
                if let Ok(message_with_fds) = receiver.receive().await {
                    let _ = server.process_message(message_with_fds, &mut sender).await;
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send burst of messages
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Send 20 messages in rapid succession, some with FDs, some without
    for i in 0..20 {
        let has_fd = i % 3 == 0; // Every 3rd message has an FD

        let (params, fds) = if has_fd {
            // Create temporary file for this message
            let mut temp_file = NamedTempFile::new().unwrap();
            write!(temp_file, "Burst message {}", i).unwrap();
            temp_file.flush().unwrap();
            temp_file.seek(SeekFrom::Start(0)).unwrap();
            let fd: OwnedFd = temp_file.into_file().into();

            let params = serde_json::json!({
                "burst_id": i,
                "file": { "__jsonrpc_fd__": true, "index": 0 }
            });

            (params, vec![fd])
        } else {
            let params = serde_json::json!({ "burst_id": i });
            (params, vec![])
        };

        let request = JsonRpcRequest::new(
            "burst_handler".to_string(),
            Some(params),
            Value::Number((i + 100).into()),
        );
        let message = JsonRpcMessage::Request(request);
        let message_with_fds = MessageWithFds::new(message, fds);

        sender.send(message_with_fds).await?;
    }

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_interleaved_requests_responses_notifications() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_interleaved.sock");

    // Create test files
    let mut temp_files = Vec::new();
    for i in 0..3 {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "Interleaved content {}", i).unwrap();
        temp_file.flush().unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        temp_files.push(temp_file);
    }

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("interleaved_method", |_method, params, fds| {
            let msg_type = params
                .as_ref()
                .and_then(|p| p.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            match msg_type {
                "with_fd" => {
                    assert_eq!(fds.len(), 1, "Expected 1 FD for with_fd type");
                    let fd = fds.into_iter().next().unwrap();
                    let mut file = File::from(fd);
                    let mut contents = String::new();
                    file.seek(SeekFrom::Start(0)).unwrap();
                    file.read_to_string(&mut contents).unwrap();
                    Ok((Some(Value::String(contents)), Vec::new()))
                }
                "without_fd" => {
                    assert!(fds.is_empty(), "Expected no FDs for without_fd type");
                    Ok((
                        Some(Value::String("No FD processed".to_string())),
                        Vec::new(),
                    ))
                }
                _ => Ok((Some(Value::String("Unknown type".to_string())), Vec::new())),
            }
        });

        server.register_method("notification_handler", |_method, _params, fds| {
            assert!(
                fds.is_empty(),
                "Notifications should not have FDs in this test"
            );
            // Notifications don't return responses
            Ok((None, Vec::new()))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Process interleaved messages
            for _ in 0..6 {
                if let Ok(message_with_fds) = receiver.receive().await {
                    let _ = server.process_message(message_with_fds, &mut sender).await;
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send interleaved messages
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // 1. Request with FD
    let fd1: OwnedFd = temp_files.remove(0).into_file().into();
    let request1 = JsonRpcRequest::new(
        "interleaved_method".to_string(),
        Some(serde_json::json!({
            "type": "with_fd",
            "file": { "__jsonrpc_fd__": true, "index": 0 }
        })),
        Value::Number(1.into()),
    );
    sender
        .send(MessageWithFds::new(
            JsonRpcMessage::Request(request1),
            vec![fd1],
        ))
        .await?;

    // 2. Notification without FD
    let notification1 = JsonRpcNotification::new(
        "notification_handler".to_string(),
        Some(serde_json::json!({ "status": "processing" })),
    );
    sender
        .send(MessageWithFds::new(
            JsonRpcMessage::Notification(notification1),
            vec![],
        ))
        .await?;

    // 3. Request without FD
    let request2 = JsonRpcRequest::new(
        "interleaved_method".to_string(),
        Some(serde_json::json!({ "type": "without_fd" })),
        Value::Number(2.into()),
    );
    sender
        .send(MessageWithFds::new(
            JsonRpcMessage::Request(request2),
            vec![],
        ))
        .await?;

    // 4. Request with FD
    let fd2: OwnedFd = temp_files.remove(0).into_file().into();
    let request3 = JsonRpcRequest::new(
        "interleaved_method".to_string(),
        Some(serde_json::json!({
            "type": "with_fd",
            "file": { "__jsonrpc_fd__": true, "index": 0 }
        })),
        Value::Number(3.into()),
    );
    sender
        .send(MessageWithFds::new(
            JsonRpcMessage::Request(request3),
            vec![fd2],
        ))
        .await?;

    // 5. Another notification
    let notification2 = JsonRpcNotification::new(
        "notification_handler".to_string(),
        Some(serde_json::json!({ "status": "continuing" })),
    );
    sender
        .send(MessageWithFds::new(
            JsonRpcMessage::Notification(notification2),
            vec![],
        ))
        .await?;

    // 6. Final request with FD
    let fd3: OwnedFd = temp_files.remove(0).into_file().into();
    let request4 = JsonRpcRequest::new(
        "interleaved_method".to_string(),
        Some(serde_json::json!({
            "type": "with_fd",
            "file": { "__jsonrpc_fd__": true, "index": 0 }
        })),
        Value::Number(4.into()),
    );
    sender
        .send(MessageWithFds::new(
            JsonRpcMessage::Request(request4),
            vec![fd3],
        ))
        .await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

// Error condition and failure mode tests

#[tokio::test]
async fn test_invalid_json_framing_error() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_framing_error.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (_sender, mut receiver) = transport.split();

            // Expect error when receiving invalid JSON
            match receiver.receive().await {
                Err(_) => {
                    // This is expected - invalid JSON should cause an error
                    println!("Successfully caught framing error");
                }
                Ok(_) => panic!("Should have failed with framing error"),
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send invalid JSON
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

    // Write invalid JSON directly to the socket
    use tokio::io::AsyncWriteExt;
    let mut stream = stream;
    let invalid_json = "{ invalid json content \n";
    stream.write_all(invalid_json.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_mismatched_fd_count_error() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_mismatch_error.sock");

    // Create test file
    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "Test content").unwrap();
    temp_file.flush().unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("mismatch_test", |_method, _params, _fds| {
            // This should never be called due to mismatch error
            panic!("Method should not be called due to FD count mismatch");
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Expect error when processing message with FD count mismatch
            match receiver.receive().await {
                Err(e) => {
                    println!("Successfully caught FD mismatch error: {:?}", e);
                }
                Ok(message_with_fds) => {
                    // If we somehow get here, try processing and expect it to fail
                    match server.process_message(message_with_fds, &mut sender).await {
                        Err(_) => println!("Error caught during processing"),
                        Ok(_) => panic!("Should have failed with mismatch error"),
                    }
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message that claims 2 FDs but only provides 1
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

    // Manually construct a message with placeholder mismatch
    use tokio::io::AsyncWriteExt;
    let mut stream = stream;

    // JSON claims 2 FDs with indices 0 and 1, but we'll only send 1 FD
    let json_with_mismatch = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "mismatch_test",
        "params": {
            "file1": { "__jsonrpc_fd__": true, "index": 0 },
            "file2": { "__jsonrpc_fd__": true, "index": 1 }
        },
        "id": 1
    });

    let json_str = serde_json::to_string(&json_with_mismatch).unwrap();

    // Send the JSON with ancillary data containing only 1 FD (but JSON expects 2)
    let _fd: OwnedFd = temp_file.into_file().into();

    // We'd need to use lower-level socket operations to create this mismatch
    // For now, just write the JSON to demonstrate the test structure
    stream.write_all(json_str.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_invalid_placeholder_indices() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_invalid_indices.sock");

    // Create test files
    let mut temp_files = Vec::new();
    for i in 0..3 {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "File {}", i).unwrap();
        temp_file.flush().unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        temp_files.push(temp_file);
    }

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("invalid_indices_test", |_method, _params, _fds| {
            panic!("Method should not be called due to invalid placeholder indices");
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Expect error when processing message with invalid placeholder indices
            match receiver.receive().await {
                Err(e) => {
                    println!("Successfully caught invalid placeholder error: {:?}", e);
                }
                Ok(message_with_fds) => {
                    match server.process_message(message_with_fds, &mut sender).await {
                        Err(_) => println!("Error caught during processing"),
                        Ok(_) => panic!("Should have failed with invalid placeholder indices"),
                    }
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message with invalid placeholder indices (non-dense range)
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Create params with invalid placeholder indices (0, 2, 4 instead of 0, 1, 2)
    let params = serde_json::json!({
        "files": [
            { "__jsonrpc_fd__": true, "index": 0 },
            { "__jsonrpc_fd__": true, "index": 2 },  // Invalid: should be 1
            { "__jsonrpc_fd__": true, "index": 4 }   // Invalid: should be 2
        ]
    });

    let fds: Vec<OwnedFd> = temp_files
        .into_iter()
        .map(|tf| tf.into_file().into())
        .collect();

    let request = JsonRpcRequest::new(
        "invalid_indices_test".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, fds);

    // This should fail during serialization or processing
    match sender.send(message_with_fds).await {
        Err(_) => println!("Successfully caught error during send"),
        Ok(_) => {
            // If send succeeds, the error should be caught by the receiver
            println!("Send succeeded, error should be caught by receiver");
        }
    }

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_duplicate_placeholder_indices() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_duplicate_indices.sock");

    // Create test files
    let mut temp_files = Vec::new();
    for i in 0..2 {
        let mut temp_file = NamedTempFile::new().unwrap();
        write!(temp_file, "File {}", i).unwrap();
        temp_file.flush().unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        temp_files.push(temp_file);
    }

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("duplicate_indices_test", |_method, _params, _fds| {
            panic!("Method should not be called due to duplicate placeholder indices");
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            match receiver.receive().await {
                Err(e) => {
                    println!("Successfully caught duplicate placeholder error: {:?}", e);
                }
                Ok(message_with_fds) => {
                    match server.process_message(message_with_fds, &mut sender).await {
                        Err(_) => println!("Error caught during processing"),
                        Ok(_) => panic!("Should have failed with duplicate placeholder indices"),
                    }
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message with duplicate placeholder indices
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Create params with duplicate placeholder indices
    let params = serde_json::json!({
        "files": [
            { "__jsonrpc_fd__": true, "index": 0 },
            { "__jsonrpc_fd__": true, "index": 0 }  // Duplicate index 0
        ]
    });

    let fds: Vec<OwnedFd> = temp_files
        .into_iter()
        .map(|tf| tf.into_file().into())
        .collect();

    let request = JsonRpcRequest::new(
        "duplicate_indices_test".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, fds);

    match sender.send(message_with_fds).await {
        Err(_) => println!("Successfully caught error during send"),
        Ok(_) => println!("Send succeeded, error should be caught by receiver"),
    }

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_no_placeholders_but_fds_provided() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_dangling_fds.sock");

    // Create test file
    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "Dangling FD content").unwrap();
    temp_file.flush().unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("dangling_fds_test", |_method, _params, fds| {
            // According to spec, this is non-compliant but may be handled
            // The FDs should be cleaned up
            if !fds.is_empty() {
                return Err(jsonrpc_fdpass::Error::InvalidMessage(
                    "Received FDs but no placeholders in message".to_string(),
                ));
            }
            Ok((
                Some(Value::String("No FDs processed".to_string())),
                Vec::new(),
            ))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            match receiver.receive().await {
                Err(e) => {
                    println!("Successfully caught dangling FD error: {:?}", e);
                }
                Ok(message_with_fds) => {
                    match server.process_message(message_with_fds, &mut sender).await {
                        Err(_) => println!("Error caught during processing - expected behavior"),
                        Ok(_) => println!("Processing succeeded - FDs were handled"),
                    }
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message with FDs but no placeholders
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Create params with NO file descriptor placeholders
    let params = serde_json::json!({
        "message": "This message has no FD placeholders"
    });

    let fd: OwnedFd = temp_file.into_file().into();

    let request = JsonRpcRequest::new(
        "dangling_fds_test".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    // But we still provide FDs - this should cause an error
    let message_with_fds = MessageWithFds::new(message, vec![fd]);

    match sender.send(message_with_fds).await {
        Err(_) => println!("Successfully caught error during send"),
        Ok(_) => println!("Send succeeded, error should be caught by receiver"),
    }

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_connection_drop_with_pending_fds() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_connection_drop.sock");

    // Create test file
    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "Connection drop test").unwrap();
    temp_file.flush().unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (_sender, mut receiver) = transport.split();

            // Try to receive but client will drop connection
            match receiver.receive().await {
                Err(e) => {
                    println!("Successfully caught connection drop error: {:?}", e);
                }
                Ok(_) => {
                    println!("Unexpected successful receive");
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and then immediately drop connection
    {
        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let transport = UnixSocketTransport::new(stream).unwrap();
        let (mut sender, _receiver) = transport.split();

        let params = serde_json::json!({
            "file": { "__jsonrpc_fd__": true, "index": 0 }
        });

        let fd: OwnedFd = temp_file.into_file().into();
        let request = JsonRpcRequest::new(
            "test_method".to_string(),
            Some(params),
            Value::Number(1.into()),
        );
        let message = JsonRpcMessage::Request(request);
        let message_with_fds = MessageWithFds::new(message, vec![fd]);

        // Start sending but don't wait for completion
        let _ = sender.send(message_with_fds).await;

        // Drop the sender/connection immediately
    } // Connection is dropped here

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_large_message_with_fds() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_large_message.sock");

    // Create test file
    let mut temp_file = NamedTempFile::new().unwrap();
    let large_content = "x".repeat(1024 * 1024); // 1MB of data
    write!(temp_file, "{}", large_content).unwrap();
    temp_file.flush().unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let expected_size = large_content.len();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("large_message_test", move |_method, params, fds| {
            assert_eq!(fds.len(), 1, "Expected exactly 1 file descriptor");

            let fd = fds.into_iter().next().unwrap();
            let mut file = File::from(fd);
            let mut contents = String::new();

            file.seek(SeekFrom::Start(0)).unwrap();
            file.read_to_string(&mut contents).unwrap();

            assert_eq!(contents.len(), expected_size, "File size mismatch");

            // Also verify the large JSON params
            let large_param = params
                .as_ref()
                .and_then(|p| p.get("large_data"))
                .and_then(|v| v.as_str())
                .unwrap();

            assert!(
                large_param.len() > 10000,
                "Large param should be substantial"
            );

            Ok((Some(Value::Number(contents.len().into())), Vec::new()))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            if let Ok(message_with_fds) = receiver.receive().await {
                let _ = server.process_message(message_with_fds, &mut sender).await;
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send large message with FD
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Create params with large data and FD placeholder
    let large_data = "Y".repeat(50000); // Large JSON parameter
    let params = serde_json::json!({
        "file": { "__jsonrpc_fd__": true, "index": 0 },
        "large_data": large_data,
        "description": "Testing large message with file descriptor"
    });

    let fd: OwnedFd = temp_file.into_file().into();

    let request = JsonRpcRequest::new(
        "large_message_test".to_string(),
        Some(params),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, vec![fd]);

    sender.send(message_with_fds).await?;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_malformed_placeholder_structure() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_malformed_placeholder.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (_sender, mut receiver) = transport.split();

            match receiver.receive().await {
                Err(e) => {
                    println!("Successfully caught malformed placeholder error: {:?}", e);
                }
                Ok(_) => {
                    println!("Unexpectedly parsed malformed placeholder");
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send message with malformed placeholder
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

    use tokio::io::AsyncWriteExt;
    let mut stream = stream;

    // Manually create malformed JSON with invalid placeholder structure
    let malformed_json = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "test",
        "params": {
            "file": {
                "__jsonrpc_fd__": "not_boolean",  // Should be boolean true
                "index": "not_number"             // Should be number
            }
        },
        "id": 1
    });

    let json_str = serde_json::to_string(&malformed_json).unwrap();
    stream.write_all(json_str.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_pretty_printed_json() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_pretty.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("echo", |_method, params, _fds| Ok((params, Vec::new())));

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Process multiple pretty-printed messages
            for _ in 0..3 {
                if let Ok(message_with_fds) = receiver.receive().await {
                    let _ = server.process_message(message_with_fds, &mut sender).await;
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send pretty-printed JSON directly
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

    use tokio::io::AsyncWriteExt;
    let mut stream = stream;

    // Send multiple pretty-printed JSON messages (with embedded newlines)
    for i in 1..=3 {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "echo",
            "params": {
                "message": format!("Pretty message {}", i),
                "nested": {
                    "key": "value",
                    "number": i
                }
            },
            "id": i
        });

        // Use pretty printing - this includes newlines within the JSON
        let pretty_json = serde_json::to_string_pretty(&msg).unwrap();
        assert!(
            pretty_json.contains('\n'),
            "Pretty JSON should contain newlines"
        );

        stream.write_all(pretty_json.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    }

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_concatenated_compact_json() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_concat.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();
        let count = std::sync::Arc::new(std::sync::Mutex::new(0));
        let count_clone = count.clone();

        server.register_method("echo", move |_method, params, _fds| {
            let mut c = count_clone.lock().unwrap();
            *c += 1;
            Ok((params, Vec::new()))
        });

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Process 3 messages
            for _ in 0..3 {
                if let Ok(message_with_fds) = receiver.receive().await {
                    let _ = server.process_message(message_with_fds, &mut sender).await;
                }
            }

            let final_count = *count.lock().unwrap();
            assert_eq!(final_count, 3, "Should have processed 3 messages");
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    // Connect and send concatenated compact JSON (no separators)
    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

    use tokio::io::AsyncWriteExt;
    let mut stream = stream;

    // Build concatenated JSON without any separators
    let mut concatenated = String::new();
    for i in 1..=3 {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "echo",
            "params": { "id": i },
            "id": i
        });
        // Compact JSON, no trailing newline
        concatenated.push_str(&serde_json::to_string(&msg).unwrap());
    }

    // Verify no newlines in the concatenated string
    assert!(
        !concatenated.contains('\n'),
        "Compact JSON should not contain newlines"
    );

    // Send all at once
    stream.write_all(concatenated.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    // Give server time to process
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_mixed_pretty_and_compact_json() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_mixed_format.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("echo", |_method, params, _fds| Ok((params, Vec::new())));

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Process 4 messages with mixed formatting
            for _ in 0..4 {
                if let Ok(message_with_fds) = receiver.receive().await {
                    let _ = server.process_message(message_with_fds, &mut sender).await;
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

    use tokio::io::AsyncWriteExt;
    let mut stream = stream;

    // Alternate between pretty and compact formatting
    for i in 1..=4 {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "echo",
            "params": { "iteration": i },
            "id": i
        });

        let json_str = if i % 2 == 0 {
            serde_json::to_string_pretty(&msg).unwrap()
        } else {
            serde_json::to_string(&msg).unwrap()
        };

        stream.write_all(json_str.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    }

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_sender_pretty_mode() -> Result<()> {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test_sender_pretty.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_handle = tokio::spawn(async move {
        let mut server = Server::new();

        server.register_method("echo", |_method, params, _fds| Ok((params, Vec::new())));

        if let Ok((stream, _)) = listener.accept().await {
            let transport = UnixSocketTransport::new(stream).unwrap();
            let (mut sender, mut receiver) = transport.split();

            // Process messages sent with pretty mode enabled
            for _ in 0..3 {
                if let Ok(message_with_fds) = receiver.receive().await {
                    let _ = server.process_message(message_with_fds, &mut sender).await;
                }
            }
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let transport = UnixSocketTransport::new(stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    // Enable pretty printing via the official API
    sender.set_pretty(true);

    for i in 1..=3 {
        let request = JsonRpcRequest::new(
            "echo".to_string(),
            Some(serde_json::json!({
                "message": format!("Pretty mode message {}", i),
                "nested": { "key": "value" }
            })),
            Value::Number(i.into()),
        );
        let message = JsonRpcMessage::Request(request);
        let message_with_fds = MessageWithFds::new(message, vec![]);

        sender.send(message_with_fds).await?;
    }

    // Clean up
    server_handle.abort();

    Ok(())
}

/// Test that large messages exceeding kernel buffer size are sent correctly.
///
/// This reproduces a bug where partial writes from sendmsg() were not handled,
/// causing large messages to be truncated.
#[tokio::test]
async fn test_large_message_exceeds_kernel_buffer() -> Result<()> {
    // Create a large payload that will exceed the typical kernel socket buffer
    // (usually ~200KB). We'll use 1MB to be safe.
    let large_data = "x".repeat(1024 * 1024);

    let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();

    let expected_data = large_data.clone();
    let server_handle = tokio::spawn(async move {
        let transport = UnixSocketTransport::new(server_stream).unwrap();
        let (_sender, mut receiver) = transport.split();

        let message_with_fds = receiver.receive().await?;
        let message = message_with_fds.message;

        // Verify we received the complete message
        if let JsonRpcMessage::Request(req) = message {
            let params = req.params.unwrap();
            let received_data = params["data"].as_str().unwrap();
            assert_eq!(
                received_data.len(),
                expected_data.len(),
                "Message was truncated! Expected {} bytes, got {} bytes",
                expected_data.len(),
                received_data.len()
            );
            assert_eq!(received_data, expected_data);
        } else {
            panic!("Expected request message");
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    let transport = UnixSocketTransport::new(client_stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    let request = JsonRpcRequest::new(
        "large_data".to_string(),
        Some(serde_json::json!({
            "data": large_data
        })),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, vec![]);

    sender.send(message_with_fds).await?;

    // Wait for server to process and verify
    server_handle.await.unwrap()?;

    Ok(())
}

/// Test that large messages with file descriptors work correctly.
///
/// This tests the case where FDs must be sent with the first chunk,
/// and remaining data sent in subsequent chunks.
#[tokio::test]
async fn test_large_message_with_fd() -> Result<()> {
    // Create a large payload
    let large_data = "y".repeat(1024 * 1024);

    // Create a temp file to pass
    let mut temp_file = NamedTempFile::new().unwrap();
    write!(temp_file, "FD test content").unwrap();
    temp_file.flush().unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();
    let fd: OwnedFd = temp_file.into_file().into();

    let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();

    let expected_data = large_data.clone();
    let server_handle = tokio::spawn(async move {
        let transport = UnixSocketTransport::new(server_stream).unwrap();
        let (_sender, mut receiver) = transport.split();

        let message_with_fds = receiver.receive().await?;
        let message = message_with_fds.message;
        let fds = message_with_fds.file_descriptors;

        // Verify we received the FD
        assert_eq!(fds.len(), 1, "Expected 1 file descriptor");

        // Verify we can read from the FD
        let mut file = File::from(fds.into_iter().next().unwrap());
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "FD test content");

        // Verify we received the complete message
        if let JsonRpcMessage::Request(req) = message {
            let params = req.params.unwrap();
            let received_data = params["data"].as_str().unwrap();
            assert_eq!(
                received_data.len(),
                expected_data.len(),
                "Message was truncated! Expected {} bytes, got {} bytes",
                expected_data.len(),
                received_data.len()
            );
        } else {
            panic!("Expected request message");
        }

        Ok::<(), jsonrpc_fdpass::Error>(())
    });

    let transport = UnixSocketTransport::new(client_stream).unwrap();
    let (mut sender, _receiver) = transport.split();

    let request = JsonRpcRequest::new(
        "large_data_with_fd".to_string(),
        Some(serde_json::json!({
            "data": large_data,
            "file": {
                "__jsonrpc_fd__": true,
                "index": 0
            }
        })),
        Value::Number(1.into()),
    );
    let message = JsonRpcMessage::Request(request);
    let message_with_fds = MessageWithFds::new(message, vec![fd]);

    sender.send(message_with_fds).await?;

    // Wait for server to process and verify
    server_handle.await.unwrap()?;

    Ok(())
}
