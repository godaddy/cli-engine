//! A scriptable loopback HTTP server for tests, standing in for an identity
//! provider's discovery and key endpoints.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

/// What an endpoint answers.
#[derive(Clone)]
pub(super) enum Reply {
    Body(String),
    Status(u16),
    Redirect(String),
    /// A redirect with a chosen status. 307 and 308 are the ones that matter for a
    /// POST: a client that follows them re-sends the request body, where a 302 is
    /// turned into a body-less GET.
    RedirectWith {
        status: u16,
        to: String,
    },
    /// A declared `content-length` of `declared` bytes, then up to `actual`
    /// bytes of body, written in chunks until the client stops reading. Only the
    /// key-set tests need it.
    #[cfg(feature = "id-token-verify")]
    Lying {
        declared: usize,
        actual: usize,
    },
    /// No `content-length` at all (the body ends when the connection closes), and
    /// up to `bytes` bytes written in chunks until the client stops reading.
    Unbounded {
        bytes: usize,
    },
}

type Routes = Arc<Mutex<HashMap<String, Reply>>>;

/// A loopback server that counts requests per path and can be re-scripted.
///
/// A path with no route answers 404, so a test that expects a request not to
/// happen can also see that it was asked for something unexpected.
pub(super) struct LoopbackServer {
    base: String,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    /// Body bytes the server managed to write, across all requests.
    sent: Arc<AtomicUsize>,
    /// The request head (request line and headers) of the latest request.
    last_head: Arc<Mutex<String>>,
    routes: Routes,
    task: JoinHandle<()>,
}

impl LoopbackServer {
    /// A server answering `reply` at `/keys` only.
    #[cfg(feature = "id-token-verify")]
    pub(super) async fn keys(reply: Reply) -> Self {
        Self::routes([("/keys", reply)]).await
    }

    /// A server answering each `(path, reply)` pair.
    pub(super) async fn routes<const N: usize>(routes: [(&str, Reply); N]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let hits = Arc::new(Mutex::new(HashMap::new()));
        let sent = Arc::new(AtomicUsize::new(0));
        let last_head = Arc::new(Mutex::new(String::new()));
        let routes: Routes = Arc::new(Mutex::new(
            routes
                .into_iter()
                .map(|(path, reply)| (path.to_owned(), reply))
                .collect(),
        ));
        let task = tokio::spawn({
            let (hits, sent, last_head, routes) = (
                hits.clone(),
                sent.clone(),
                last_head.clone(),
                routes.clone(),
            );
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(
                        stream,
                        hits.clone(),
                        sent.clone(),
                        last_head.clone(),
                        routes.clone(),
                    ));
                }
            }
        });
        Self {
            base,
            hits,
            sent,
            last_head,
            routes,
            task,
        }
    }

    /// The URL of `path` on this server.
    pub(super) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The URL of `/keys`.
    #[cfg(feature = "id-token-verify")]
    pub(super) fn keys_url(&self) -> String {
        self.url("/keys")
    }

    /// Requests received for `path`.
    pub(super) fn hits_for(&self, path: &str) -> usize {
        self.hits
            .lock()
            .expect("hits lock")
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    /// Requests received for any path.
    pub(super) fn hits(&self) -> usize {
        self.hits.lock().expect("hits lock").values().sum()
    }

    pub(super) fn body_bytes_sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }

    /// The headers of the latest request, lowercased, one `name: value` per line.
    pub(super) fn last_request_headers(&self) -> String {
        self.last_head
            .lock()
            .expect("head lock")
            .to_ascii_lowercase()
    }

    /// Changes what `path` answers from now on.
    pub(super) fn set(&self, path: &str, reply: Reply) {
        self.routes
            .lock()
            .expect("routes lock")
            .insert(path.to_owned(), reply);
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut stream: TcpStream,
    hits: Arc<Mutex<HashMap<String, usize>>>,
    sent: Arc<AtomicUsize>,
    last_head: Arc<Mutex<String>>,
    routes: Routes,
) {
    let mut head = Vec::new();
    let mut buf = [0_u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(read) => head.extend_from_slice(&buf[..read]),
        }
    }
    let head_text = String::from_utf8_lossy(&head).into_owned();
    let path = head_text
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_owned();
    *hits
        .lock()
        .expect("hits lock")
        .entry(path.clone())
        .or_insert(0) += 1;
    *last_head.lock().expect("head lock") = head_text;
    let reply = routes
        .lock()
        .expect("routes lock")
        .get(&path)
        .cloned()
        .unwrap_or(Reply::Status(404));
    let response = match reply {
        Reply::Body(body) => format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
        Reply::Status(code) => {
            format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        }
        Reply::Redirect(to) => format!(
            "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\n\
             connection: close\r\n\r\n"
        ),
        Reply::RedirectWith { status, to } => format!(
            "HTTP/1.1 {status} Redirect\r\nlocation: {to}\r\ncontent-length: 0\r\n\
             connection: close\r\n\r\n"
        ),
        #[cfg(feature = "id-token-verify")]
        Reply::Lying { declared, actual } => {
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {declared}\r\nconnection: close\r\n\r\n"
            );
            stream_body(&mut stream, head, actual, &sent).await;
            return;
        }
        Reply::Unbounded { bytes } => {
            let head = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_owned();
            stream_body(&mut stream, head, bytes, &sent).await;
            return;
        }
    };
    // A client that already gave up makes this fail; that is expected in the
    // tests that abandon a request.
    drop(stream.write_all(response.as_bytes()).await);
}

/// Writes `head`, then up to `total` bytes of body in 16 KiB chunks, counting
/// what was accepted. Stops at the first write error, which is the client closing
/// the connection once it has decided to give up.
async fn stream_body(stream: &mut TcpStream, head: String, total: usize, sent: &AtomicUsize) {
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let chunk = vec![b'x'; 16 * 1024];
    let mut remaining = total;
    while remaining > 0 {
        let n = remaining.min(chunk.len());
        if stream.write_all(&chunk[..n]).await.is_err() {
            return;
        }
        sent.fetch_add(n, Ordering::SeqCst);
        remaining -= n;
    }
}
