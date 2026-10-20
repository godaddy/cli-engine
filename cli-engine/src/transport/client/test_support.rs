//! Loopback HTTP server shared by the transport client tests.

use std::{
    io::{Read, Write},
    net::TcpListener,
    thread::{self, JoinHandle},
    time::Duration,
};

/// Serves one request on loopback and returns its URL and a handle yielding the
/// request head (the bytes up to the blank line that ends the headers).
pub(super) fn serve_once() -> (String, JoinHandle<String>) {
    serve_once_after(Duration::ZERO)
}

/// Like [`serve_once`], but waits `delay` after reading the request head before
/// responding.
///
/// TCP may deliver the head across several reads, so this reads until the
/// terminator arrives. After responding it drains any request body still in
/// flight, so closing the socket never resets the client mid-write.
pub(super) fn serve_once_after(delay: Duration) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let url = format!("http://{}/", listener.local_addr().expect("local addr"));
    let handle = thread::spawn(move || {
        const HEAD_END: &[u8] = b"\r\n\r\n";
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut received = Vec::new();
        let mut buf = [0_u8; 4096];
        let head_len = loop {
            if let Some(at) = received.windows(HEAD_END.len()).position(|w| w == HEAD_END) {
                break at;
            }
            let read = stream.read(&mut buf).expect("read request head");
            assert!(read > 0, "connection closed before the request head ended");
            received.extend_from_slice(&buf[..read]);
        };
        thread::sleep(delay);
        let responded =
            stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        // A client that already gave up (the timeout under test) makes the
        // write fail; that is expected, not a test error.
        if responded.is_err() {
            return String::from_utf8_lossy(&received[..head_len]).into_owned();
        }
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set drain timeout");
        while matches!(stream.read(&mut buf), Ok(read) if read > 0) {}
        String::from_utf8_lossy(&received[..head_len]).into_owned()
    });
    (url, handle)
}
