//! A request written to a pooled connection the peer already closed is re-sent once on a fresh
//! connection; anything else that fails without a response is not.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;

/// Read one request (head and `content-length` body); `false` when the peer closed first.
async fn read_one_request(socket: &mut TcpStream) -> bool {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => return false,
        }
    }
    let length = String::from_utf8_lossy(&head)
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    socket.read_exact(&mut body).await.is_ok()
}

async fn respond(socket: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
}

fn post(url: String) -> RequestOptions {
    RequestOptions {
        body: Some("{}".to_string()),
        ..RequestOptions::new(reqwest::Method::POST, url)
    }
}

/// The first connection answers one request, then closes without answering the next (the edge
/// closing a kept-alive connection under the pool); a fresh connection answers. The second
/// request still succeeds, over exactly one new connection.
#[tokio::test]
async fn a_request_on_a_connection_the_peer_closed_is_resent_on_a_fresh_one() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&connections);
    tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        accepted.fetch_add(1, Ordering::SeqCst);
        assert!(read_one_request(&mut first).await);
        respond(&mut first, "first").await;
        // The pooled connection receives the next request and dies without a response.
        read_one_request(&mut first).await;
        drop(first);
        let (mut fresh, _) = listener.accept().await.unwrap();
        accepted.fetch_add(1, Ordering::SeqCst);
        assert!(read_one_request(&mut fresh).await);
        respond(&mut fresh, "second").await;
    });

    let mut first = send(post(url.clone())).await.unwrap();
    assert_eq!(first.read_all_text().await.unwrap(), "first");
    let mut second = send(post(url)).await.unwrap();
    assert_eq!(second.status, 200);
    assert_eq!(second.read_all_text().await.unwrap(), "second");
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}

/// A peer that closes every connection unanswered gets exactly one fresh retry; the failure then
/// surfaces as the SDK's connection error, its cause the transport chain without the URL (whose
/// query may carry a credential).
#[tokio::test]
async fn a_peer_that_keeps_closing_gets_one_retry_and_a_cause_without_the_url() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/messages?key=query-credential",
        listener.local_addr().unwrap()
    );
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&connections);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            accepted.fetch_add(1, Ordering::SeqCst);
            read_one_request(&mut socket).await;
        }
    });

    let Err(ProviderError::Connection(error)) = send(post(url)).await else {
        panic!("a request every peer closes must fail as a connection error");
    };
    assert_eq!(error.kind, ConnectionErrorKind::Reset);
    assert_eq!(error.message(), "Connection error.");
    assert!(
        error
            .cause
            .contains("connection closed before message completed"),
        "{}",
        error.cause
    );
    assert!(!error.cause.contains("query-credential"), "{}", error.cause);
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}

/// A refused connect is not a stale connection: the network said no, and a fresh connection would
/// meet the same answer, so the request fails once.
#[tokio::test]
async fn a_refused_connect_is_not_retried() {
    // Bound but not listening: connects are refused while the socket holds the port.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!("http://{}/v1/messages", socket.local_addr().unwrap());

    let Err(ProviderError::Connection(error)) = send(post(url)).await else {
        panic!("a refused connect must fail as a connection error");
    };
    assert_eq!(error.kind, ConnectionErrorKind::Connect);
    assert_eq!(
        error.transport_failure().class,
        crate::utils_inner::stream_failure::TransportFailureClass::Connect
    );
}
