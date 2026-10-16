//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Test helpers for endpoint-plugin authors.
//!
//! A plugin's real test is loading the compiled artifact, so a test needs the
//! path of a freshly built `cdylib`. [`build_plugin_cdylib`] produces it by
//! building the package and reading the artifact path back out of cargo, which
//! keeps the test independent of target directory layout and file extensions.
//!
//! ```no_run
//! # #[tokio::test]
//! # async fn plugin_round_trip() -> anyhow::Result<()> {
//! let library = mq_bridge::plugin::test_support::build_plugin_cdylib(".", "mq-bridge-pulsar")?;
//! mq_bridge::plugin::load_endpoint_plugin(&library)?;
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};

use crate::traits::MessageConsumer;
use crate::{CanonicalMessage, ReceivedBatch};

/// Builds `package` as a shared library and returns the artifact path.
///
/// `manifest_dir` is any directory inside the package's workspace. The build
/// uses the same profile as the running test, so a `cargo test --release` run
/// loads a release plugin.
pub fn build_plugin_cdylib(
    manifest_dir: impl AsRef<Path>,
    package: &str,
) -> anyhow::Result<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command
        .current_dir(manifest_dir.as_ref())
        .args([
            "build",
            "--message-format=json-render-diagnostics",
            "--package",
            package,
        ])
        // Nested cargo runs inherit the outer build's flags otherwise, which
        // rebuilds the world under a different fingerprint.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS");
    if !cfg!(debug_assertions) {
        command.arg("--release");
    }

    let output = command
        .output()
        .with_context(|| format!("failed to run cargo to build plugin `{package}`"))?;
    if !output.status.success() {
        bail!(
            "building plugin `{package}` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    artifact_path(&output.stdout, package).ok_or_else(|| {
        anyhow!(
            "cargo built `{package}` but produced no cdylib artifact; \
             add `crate-type = [\"cdylib\"]` to its [lib] section"
        )
    })
}

/// Receives one non-empty batch and leaves it uncommitted. Panics after `timeout`.
pub async fn receive_one_batch(
    consumer: &mut dyn MessageConsumer,
    timeout: Duration,
) -> ReceivedBatch {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let batch = tokio::time::timeout(remaining, consumer.receive_batch(16))
            .await
            .unwrap_or_else(|_| panic!("no message arrived within {timeout:?}"))
            .expect("receive batch");
        if !batch.messages.is_empty() {
            return batch;
        }
        assert!(
            Instant::now() < deadline,
            "no message arrived within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Receives until at least `expected` messages arrived, committing nothing.
/// Panics after `timeout`.
pub async fn receive_at_least(
    consumer: &mut dyn MessageConsumer,
    expected: usize,
    timeout: Duration,
) -> Vec<CanonicalMessage> {
    let deadline = Instant::now() + timeout;
    let mut messages = Vec::new();
    while messages.len() < expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let batch = tokio::time::timeout(remaining, consumer.receive_batch(16))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "only {} of {expected} messages arrived within {timeout:?}",
                    messages.len()
                )
            })
            .expect("receive batch");
        messages.extend(batch.messages);
        assert!(
            Instant::now() < deadline,
            "only {} of {expected} messages arrived within {timeout:?}",
            messages.len()
        );
        if messages.len() < expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    messages
}

/// The payloads as text, in order.
pub fn payload_texts<'a>(messages: impl IntoIterator<Item = &'a CanonicalMessage>) -> Vec<String> {
    messages
        .into_iter()
        .map(|message| message.get_payload_str().into_owned())
        .collect()
}

/// One request a [`StubHttpServer`] received.
#[derive(Debug, Clone)]
pub struct StubRequest {
    pub method: String,
    /// Path and query, as sent.
    pub target: String,
    /// Header names are lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl StubRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Status, extra headers and body of a stub answer.
pub type StubAnswer = (u16, Vec<(String, String)>, String);

/// A local HTTP/1.1 server standing in for the service a sink talks to, so a
/// unit test can script its answers — a slow job, a 503, a rejected document —
/// without Docker. `respond` returns a status and a JSON body for each request;
/// every request is kept for [`requests`](Self::requests). It reads
/// `Content-Length` bodies only and closes the connection after each answer.
///
/// ```no_run
/// # async fn example() -> std::io::Result<()> {
/// use mq_bridge::plugin::test_support::StubHttpServer;
/// let server = StubHttpServer::start(|request| match request.method.as_str() {
///     "POST" => (202, r#"{"taskUid":7}"#.to_string()),
///     _ => (200, r#"{"status":"succeeded"}"#.to_string()),
/// })
/// .await?;
/// // point the endpoint at server.url(), then assert on server.requests()
/// # Ok(())
/// # }
/// ```
pub struct StubHttpServer {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<StubRequest>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl StubHttpServer {
    pub async fn start(
        respond: impl Fn(&StubRequest) -> (u16, String) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        Self::start_with_headers(move |request| {
            let (status, body) = respond(request);
            (status, Vec::new(), body)
        })
        .await
    }

    /// Like [`start`](Self::start), with extra response headers.
    pub async fn start_with_headers(
        respond: impl Fn(&StubRequest) -> StubAnswer + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&requests);
        let respond = std::sync::Arc::new(respond);
        let accept = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (seen, respond) = (seen.clone(), respond.clone());
                tokio::spawn(async move {
                    let _ = serve_stub(socket, &seen, &*respond).await;
                });
            }
        });
        Ok(Self {
            url,
            requests,
            accept,
        })
    }

    /// `http://127.0.0.1:<port>`, without a trailing slash.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<StubRequest> {
        self.requests
            .lock()
            .expect("stub requests poisoned")
            .clone()
    }
}

impl Drop for StubHttpServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn serve_stub(
    mut socket: tokio::net::TcpStream,
    seen: &std::sync::Mutex<Vec<StubRequest>>,
    respond: &(dyn Fn(&StubRequest) -> StubAnswer + Send + Sync),
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut received = Vec::new();
    let mut buffer = [0u8; 8192];
    let head_end = loop {
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        received.extend_from_slice(&buffer[..read]);
    };
    let head = String::from_utf8_lossy(&received[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_string();
    let target = request_line.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = received.split_off(head_end + 4);
    while body.len() < length {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    let request = StubRequest {
        method,
        target,
        headers,
        body,
    };
    let (status, extra, answer) = respond(&request);
    seen.lock().expect("stub requests poisoned").push(request);
    let extra: String = extra
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let response = format!(
        "HTTP/1.1 {status} Stub\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
        answer.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}

/// Picks the package's shared-library artifact out of cargo's JSON message stream.
fn artifact_path(stdout: &[u8], package: &str) -> Option<PathBuf> {
    let wanted = package.replace('-', "_");
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| {
            message["target"]["name"]
                .as_str()
                .is_some_and(|name| name.replace('-', "_") == wanted)
        })
        .filter(|message| {
            message["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"))
        })
        .filter_map(|message| {
            message["filenames"]
                .as_array()?
                .iter()
                .filter_map(|name| name.as_str())
                .find(|name| is_shared_library(name))
                .map(PathBuf::from)
        })
        .next_back()
}

fn is_shared_library(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "so" | "dylib" | "dll"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_stub_server_answers_and_records_a_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let server =
            StubHttpServer::start(|request| (202, format!("{{\"len\":{}}}", request.body.len())))
                .await
                .unwrap();
        let address = server.url().trim_start_matches("http://").to_string();
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"POST /indexes/a/documents?x=1 HTTP/1.1\r\nHost: stub\r\nContent-Length: 5\r\nX-Key: k\r\n\r\nhello")
            .await
            .unwrap();
        let mut answer = String::new();
        socket.read_to_string(&mut answer).await.unwrap();

        assert!(answer.starts_with("HTTP/1.1 202"), "{answer}");
        assert!(answer.ends_with("{\"len\":5}"), "{answer}");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].target, "/indexes/a/documents?x=1");
        assert_eq!(requests[0].header("x-key"), Some("k"));
        assert_eq!(requests[0].body, b"hello");
    }

    #[test]
    fn artifact_path_picks_the_packages_shared_library() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"other","kind":["cdylib"]},"filenames":["/t/libother.so"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"my-plugin","kind":["lib","cdylib"]},"filenames":["/t/libmy_plugin.rlib","/t/libmy_plugin.dylib"]}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );
        assert_eq!(
            artifact_path(stdout.as_bytes(), "my-plugin"),
            Some(PathBuf::from("/t/libmy_plugin.dylib"))
        );
    }

    #[test]
    fn artifact_path_ignores_rlib_only_packages() {
        let stdout = r#"{"reason":"compiler-artifact","target":{"name":"plain","kind":["lib"]},"filenames":["/t/libplain.rlib"]}"#;
        assert_eq!(artifact_path(stdout.as_bytes(), "plain"), None);
    }
}
