//! Test-only loopback HTTP/1.1 server for provider wire tests: it captures each request (head and
//! body) and answers with a scripted response whose body goes out as separate chunked-encoding
//! frames (so a test controls exactly where the client's body chunks split), optionally holding
//! the connection open after the last frame to model a provider stream that goes silent.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One scripted response.
pub(crate) struct MockResponse {
    pub status: u16,
    pub content_type: &'static str,
    /// Body frames, each written as its own chunked-encoding chunk.
    pub frames: Vec<Vec<u8>>,
    /// Keep the connection open after the last frame instead of terminating the body.
    pub hold_open: bool,
}

impl MockResponse {
    /// A complete `text/event-stream` 200 response sent as one frame.
    pub fn sse(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            frames: vec![body.into().into_bytes()],
            hold_open: false,
        }
    }
}

/// A captured request.
#[derive(Clone, Debug)]
pub(crate) struct CapturedRequest {
    pub head: String,
    pub body: Vec<u8>,
}

impl CapturedRequest {
    /// The value of a request header (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }
}

/// A running mock server.
pub(crate) struct MockHttp {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl MockHttp {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The requests captured so far, in arrival order.
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("mock requests lock").clone()
    }
}

/// Serve the scripted responses, one per accepted connection, in order.
pub(crate) async fn serve(responses: Vec<MockResponse>) -> MockHttp {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock bind");
    let addr = listener.local_addr().expect("mock addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.expect("mock accept");
            let request = read_request(&mut socket).await;
            captured.lock().expect("mock requests lock").push(request);
            let reason = if response.status < 400 { "OK" } else { "Error" };
            let head = format!(
                "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\n\r\n",
                response.status, response.content_type
            );
            socket.write_all(head.as_bytes()).await.expect("mock head");
            for frame in &response.frames {
                let mut chunk = format!("{:x}\r\n", frame.len()).into_bytes();
                chunk.extend_from_slice(frame);
                chunk.extend_from_slice(b"\r\n");
                socket.write_all(&chunk).await.expect("mock frame");
                socket.flush().await.expect("mock flush");
            }
            if response.hold_open {
                // Silent until the client gives up and closes the connection.
                let mut sink = [0u8; 64];
                while matches!(socket.read(&mut sink).await, Ok(n) if n > 0) {}
            } else {
                socket.write_all(b"0\r\n\r\n").await.expect("mock end");
            }
        }
    });
    MockHttp { addr, requests }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    while !buffer.ends_with(b"\r\n\r\n") {
        socket
            .read_exact(&mut byte)
            .await
            .expect("mock request head");
        buffer.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buffer).to_string();
    let length = head
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
    socket
        .read_exact(&mut body)
        .await
        .expect("mock request body");
    CapturedRequest { head, body }
}
