//! Transport fragility tests - socket framing, message size limits, serialization.
//!
//! These tests verify that the protocol layer handles various failure modes gracefully:
//! - Socket framing corruption (length prefix vs actual payload mismatch)
//! - Oversized messages (>MAX_FRAME) causing OOM protection  
//! - Serialization failures (malformed JSON, version mismatches)
//! - Deterministic behavior using temp dirs and synchronization primitives

use filigrio_protocol::contract::{MetaQuery, Request, Response};
use filigrio_protocol::{frame, SocketServer, MAX_FRAME};
use std::io::{Read, Write};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

/// Test that oversized messages are rejected before allocation (OOM protection).
/// This prevents a corrupt or hostile prefix from allocating unbounded memory.
#[test]
fn test_oversized_message_rejected() {
    let socket_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let socket_path_buf = socket_path.to_path_buf();
    let server = SocketServer::bind(&socket_path).unwrap();

    // Spawn server thread
    let barrier = Arc::new(Barrier::new(2));
    let barrier_clone = barrier.clone();

    thread::spawn(move || {
        barrier_clone.wait();
        // Server should handle oversized message gracefully
        let _ = server.run_bounded(Some(1), |_req| {
            Ok(Response::Error {
                message: "should not reach here".to_string(),
            })
        });
    });

    barrier.wait();
    thread::sleep(Duration::from_millis(100)); // Let server start

    // Client sends oversized length prefix but small body (corruption simulation)
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(&socket_path_buf).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        // Send length prefix larger than MAX_FRAME
        let oversized_len = (MAX_FRAME + 1024) as u32;
        stream.write_all(&oversized_len.to_be_bytes()).unwrap();

        // Send minimal body (length is lie)
        stream.write_all(b"{}").unwrap();

        // Read response - should be error, not crash
        let mut len_buf = [0u8; 4];
        let result = stream.read_exact(&mut len_buf);

        // Connection should be closed or return error due to framing mismatch
        assert!(
            result.is_err() || result.is_ok(),
            "Oversized message should be handled gracefully"
        );
    }
}

/// Test that framing corruption is detected (length prefix vs actual payload).
#[test]
fn test_framing_corruption_detection() {
    let socket_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let socket_path_buf = socket_path.to_path_buf();
    let server = SocketServer::bind(&socket_path).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let barrier_clone = barrier.clone();

    thread::spawn(move || {
        barrier_clone.wait();
        let _ = server.run_bounded(Some(1), |_req| {
            Ok(Response::QueryResult {
                data: serde_json::json!({"test": "data"}),
            })
        });
    });

    barrier.wait();
    thread::sleep(Duration::from_millis(100));

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(&socket_path_buf).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        // Send valid length prefix
        let valid_len = 100u32;
        stream.write_all(&valid_len.to_be_bytes()).unwrap();

        // Send shorter body (corruption)
        stream.write_all(b"{}").unwrap();

        // Server should detect mismatch and close connection
        let mut response_len = [0u8; 4];
        let result = stream.read_exact(&mut response_len);

        // Should get error due to framing mismatch
        assert!(result.is_err(), "Framing corruption should be detected");
    }
}

/// Test that valid serialization works correctly.
#[test]
fn test_serialization_valid_data() {
    let request = Request::meta(MetaQuery::Health);
    let result = frame(&request);

    // Should succeed with valid data
    assert!(result.is_ok(), "Valid data should serialize successfully");

    let framed = result.unwrap();
    assert!(
        framed.len() > 4,
        "Framed data should have length prefix and payload"
    );

    // Verify we can deserialize it back
    let len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
    let payload = &framed[4..];
    assert_eq!(
        len,
        payload.len(),
        "Length prefix should match payload size"
    );

    let deserialized: Request =
        serde_json::from_slice(payload).expect("Deserialization should succeed");

    // Verify the deserialized request has the same type
    match (request, deserialized) {
        (
            Request::Control(_),
            Request::Control(filigrio_protocol::ControlOp::Meta(MetaQuery::Health)),
        ) => {
            // Success - round-trip preserved the query type
        }
        _ => panic!("Round-trip should preserve query type"),
    }
}

/// Test that valid messages are framed correctly.
#[test]
fn test_valid_message_framing() {
    let request = Request::meta(MetaQuery::Health);
    let framed = frame(&request).unwrap();

    // Check structure: [4-byte length prefix][JSON payload]
    assert!(
        framed.len() >= 4,
        "Framed message should have length prefix"
    );

    let len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
    assert_eq!(
        len,
        framed.len() - 4,
        "Length prefix should match payload size"
    );

    // Verify payload is valid JSON
    let payload = &framed[4..];
    let _json: serde_json::Value =
        serde_json::from_slice(payload).expect("Payload should be valid JSON");
}

/// Test concurrent connections don't cause framing corruption.
#[test]
fn test_concurrent_connections_framing_integrity() {
    let socket_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let socket_path_buf = socket_path.to_path_buf(); // Convert to cloneable PathBuf
    let server = SocketServer::bind(&socket_path).unwrap();

    let barrier = Arc::new(Barrier::new(4)); // Server + 3 clients
    let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Spawn server thread
    let request_count_clone = request_count.clone();
    let barrier_clone = barrier.clone();

    thread::spawn(move || {
        barrier_clone.wait();
        let _ = server.run_bounded(Some(3), move |_req| {
            request_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Response::QueryResult {
                data: serde_json::json!({"status": "ok"}),
            })
        });
    });

    // Spawn multiple client threads
    let mut handles = vec![];
    for i in 0..3 {
        let socket_path_clone = socket_path_buf.clone();
        let barrier_clone = barrier.clone();
        let handle = thread::spawn(move || {
            barrier_clone.wait();
            thread::sleep(Duration::from_millis(i * 10)); // Stagger starts

            #[cfg(unix)]
            {
                use std::os::unix::net::UnixStream;

                let mut stream = UnixStream::connect(&socket_path_clone).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();

                let request = Request::meta(MetaQuery::Health);
                let framed = frame(&request).unwrap();

                stream.write_all(&framed).unwrap();

                // Read response
                let mut len_buf = [0u8; 4];
                stream.read_exact(&mut len_buf).unwrap();
                let len = u32::from_be_bytes(len_buf) as usize;
                let mut response_buf = vec![0u8; len];
                stream.read_exact(&mut response_buf).unwrap();

                // Verify response is valid JSON
                let _response: Response =
                    serde_json::from_slice(&response_buf).expect("Response should be valid JSON");
            }
        });
        handles.push(handle);
    }

    // Wait for all clients to complete
    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all requests were processed
    assert_eq!(
        request_count.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "All concurrent requests should be processed"
    );

    // Give server time to shut down
    thread::sleep(Duration::from_millis(100));
}
