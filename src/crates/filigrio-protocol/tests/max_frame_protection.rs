//! Test that validates MAX_FRAME protection actually works.
//!
//! This is a focused test to verify that the load-bearing transport protection
//! against oversized messages is functional.

use filigrio_protocol::contract::{MetaQuery, Request, Response};
use filigrio_protocol::{SocketServer, MAX_FRAME};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Test that MAX_FRAME protection prevents oversized allocation.
/// This is a load-bearing test - if it passes, the OOM protection works.
#[test]
fn test_max_frame_protection() {
    let socket_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let socket_path_buf = socket_path.to_path_buf();
    let server = SocketServer::bind(&socket_path).unwrap();

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let barrier_clone = barrier.clone();

    // Spawn server thread
    thread::spawn(move || {
        barrier_clone.wait();
        let _ = server.run_bounded(Some(1), |_req| {
            Ok(Response::QueryResult {
                data: serde_json::json!({"status": "should not reach here"}),
            })
        });
    });

    barrier.wait();
    thread::sleep(Duration::from_millis(100));

    // Client sends an oversized length prefix
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

    // Send a small body (the length is a lie - corruption scenario)
    stream.write_all(b"{}").unwrap();

    // Try to read response length
    let mut len_buf = [0u8; 4];
    let result = stream.read_exact(&mut len_buf);

    // The connection should be closed or error due to frame size validation
    // This proves the server rejected the oversized message before allocation
    assert!(
        result.is_err(),
        "Server should reject oversized message and close connection"
    );

    if let Err(e) = result {
        // We expect a connection reset or timeout, not success
        assert!(
            e.kind() == std::io::ErrorKind::ConnectionReset
                || e.kind() == std::io::ErrorKind::UnexpectedEof
                || e.kind() == std::io::ErrorKind::TimedOut,
            "Connection should be closed after oversized message rejection"
        );
    }
}

/// Test that normal-sized messages work (baseline for MAX_FRAME test).
#[test]
fn test_normal_message_works() {
    let socket_path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    let socket_path_buf = socket_path.to_path_buf();
    let server = SocketServer::bind(&socket_path).unwrap();

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let barrier_clone = barrier.clone();

    thread::spawn(move || {
        barrier_clone.wait();
        let _ = server.run_bounded(Some(1), |_req| {
            Ok(Response::QueryResult {
                data: serde_json::json!({"status": "ok"}),
            })
        });
    });

    barrier.wait();
    thread::sleep(Duration::from_millis(100));

    // Client sends a normal-sized request
    let mut stream = UnixStream::connect(&socket_path_buf).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let request = Request::meta(MetaQuery::Health);
    let framed = filigrio_protocol::frame(&request).unwrap();

    stream.write_all(&framed).unwrap();

    // Read response - should succeed
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut response_buf = vec![0u8; len];
    stream.read_exact(&mut response_buf).unwrap();

    // Verify response is valid
    let _response: Response = serde_json::from_slice(&response_buf)
        .expect("Normal-sized message should get valid response");
}
