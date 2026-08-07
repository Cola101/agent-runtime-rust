use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

#[derive(Debug)]
#[allow(dead_code)]
pub struct CapturedRequest {
    pub head: String,
    pub body: Value,
}

#[allow(dead_code)]
pub async fn spawn_sse_server(
    path: &'static str,
    response_body: &'static str,
) -> (
    String,
    oneshot::Receiver<CapturedRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let captured = read_request(&mut socket).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        captured_tx.send(captured).ok();
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (format!("http://{address}{path}"), captured_rx, server)
}

#[allow(dead_code)]
pub async fn spawn_http_server(
    path: &'static str,
    status: u16,
    response_body: &'static str,
) -> (
    String,
    oneshot::Receiver<CapturedRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let captured = read_request(&mut socket).await;
        let reason = if status == 200 { "OK" } else { "Error" };
        let content_type = if status == 200 {
            "text/event-stream"
        } else {
            "application/json"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        captured_tx.send(captured).ok();
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (format!("http://{address}{path}"), captured_rx, server)
}

#[allow(dead_code)]
pub async fn spawn_streaming_then_stall_server(
    path: &'static str,
    response_body: &'static str,
    stall: std::time::Duration,
) -> (
    String,
    oneshot::Receiver<CapturedRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (captured_tx, captured_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let captured = read_request(&mut socket).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{response_body}"
        );
        captured_tx.send(captured).ok();
        socket.write_all(response.as_bytes()).await.unwrap();
        tokio::time::sleep(stall).await;
    });
    (format!("http://{address}{path}"), captured_rx, server)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request closed before headers completed");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8(buffer[..header_end].to_vec()).unwrap();
    let content_length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .map(str::parse::<usize>)
        })
        .unwrap()
        .unwrap();
    while buffer.len() - header_end < content_length {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request closed before body completed");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&buffer[header_end..header_end + content_length]).unwrap();
    CapturedRequest { head, body }
}
