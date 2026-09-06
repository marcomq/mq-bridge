//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;

/// Persistent, connection-pooling hyper client used by `HttpPublisher`.
type HttpClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    http_body_util::Full<Bytes>,
>;

/// Cap on how much of a failed response body is quoted in the error. Bodies may be up to
/// [`MAX_HTTP_BODY_BYTES`]; an error string carried through retries and logs must not be.
const MAX_ERROR_BODY_EXCERPT_BYTES: usize = 2048;

/// Renders the leading bytes of a response body for an error message, truncating on a UTF-8
/// character boundary so a multi-byte character is never split.
fn error_body_excerpt(body: &[u8]) -> String {
    if body.len() <= MAX_ERROR_BODY_EXCERPT_BYTES {
        return String::from_utf8_lossy(body).into_owned();
    }
    // Back off any UTF-8 continuation bytes (0b10xxxxxx) so the cut lands between characters.
    let mut end = MAX_ERROR_BODY_EXCERPT_BYTES;
    while end > 0 && body[end] & 0b1100_0000 == 0b1000_0000 {
        end -= 1;
    }
    format!(
        "{}... ({} bytes truncated)",
        String::from_utf8_lossy(&body[..end]),
        body.len() - end
    )
}

/// Builds a connection-pooling hyper client for the given TLS/connector settings.
fn build_http_client(config: &HttpConfig) -> anyhow::Result<HttpClient> {
    let tls_client_config = create_rustls_client_config(&config.tls)
        .context("Failed to create rustls client config")?;

    let mut http_connector = HttpConnector::new();
    http_connector.enforce_http(false);
    http_connector.set_nodelay(true);
    if let Some(keepalive) = config.tcp_keepalive_ms {
        http_connector.set_keepalive(Some(std::time::Duration::from_millis(keepalive)));
    }

    // Handles both http and https, and http1/http2.
    let https_connector = HttpsConnectorBuilder::new()
        .with_tls_config(tls_client_config)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http_connector);

    let mut client_builder = hyper_util::client::legacy::Client::builder(TokioExecutor::new());
    if let Some(timeout) = config.pool_idle_timeout_ms {
        client_builder.pool_idle_timeout(std::time::Duration::from_millis(timeout));
    }
    Ok(client_builder.build(https_connector))
}

/// A set of independent pooled hyper clients that requests are round-robined across.
///
/// A single `legacy::Client` guards its connection pool with one mutex, so under high
/// concurrency every in-flight request serialises on that lock — which caps publisher
/// throughput and, unlike the server side (no shared lock), does not scale with cores.
/// Sharding across several clients spreads that contention so send throughput scales.
struct ShardedHttpClient {
    clients: Vec<HttpClient>,
    next: std::sync::atomic::AtomicUsize,
}

impl ShardedHttpClient {
    /// Picks the next client, round-robin. Single-shard fast path avoids the atomic.
    #[inline]
    fn pick(&self) -> &HttpClient {
        if self.clients.len() == 1 {
            return &self.clients[0];
        }
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.clients.len();
        &self.clients[idx]
    }
}

/// Number of pooled clients to shard across — matches available parallelism (capped) so
/// pool-lock contention scales with cores, like the server side does.
fn http_client_shard_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 16)
}

/// Builds a sharded set of connection-pooling hyper clients for these connector settings.
fn build_sharded_http_client(config: &HttpConfig) -> anyhow::Result<ShardedHttpClient> {
    let shards = http_client_shard_count();
    let mut clients = Vec::with_capacity(shards);
    for _ in 0..shards {
        clients.push(build_http_client(config)?);
    }
    Ok(ShardedHttpClient {
        clients,
        next: std::sync::atomic::AtomicUsize::new(0),
    })
}

/// Returns a shared sharded HTTP client for these client-level settings, building one on
/// first use. The request URL is per-message, so one client serves all targets it reaches.
async fn create_shared_http_client(
    config: &HttpConfig,
) -> anyhow::Result<std::sync::Arc<ShardedHttpClient>> {
    let identity = crate::support::connection_registry::connection_identity((
        config.tls.required,
        &config.tls.ca_file,
        &config.tls.cert_file,
        &config.tls.key_file,
        config.tls.accept_invalid_certs,
        config.tcp_keepalive_ms,
        config.pool_idle_timeout_ms,
    ));
    let config_clone = config.clone();
    crate::support::connection_registry::get_or_create(
        "http-client",
        identity,
        config.shared.unwrap_or(true),
        move || async move { build_sharded_http_client(&config_clone) },
    )
    .await
}

/// A publisher that sends messages to an HTTP endpoint using hyper.
///
/// Features:
/// - Connection pooling for both HTTP and HTTPS via `hyper-rustls`.
/// - Automatic negotiation of HTTP/1.1 or HTTP/2.
/// - Configurable request timeout and batch concurrency
/// - Comprehensive tracing for sent messages and response status
/// - Proper error classification (Retryable vs NonRetryable)
#[derive(Clone)]
pub struct HttpPublisher {
    /// Persistent HTTP client with connection pooling
    client: std::sync::Arc<ShardedHttpClient>,
    url: String,
    base_uri: hyper::Uri,
    /// Default HTTP method to use if not overridden by message metadata
    method: hyper::Method,
    request_timeout: std::time::Duration,
    batch_concurrency: usize,
    compression: Compression,
    compression_threshold_bytes: usize,
    basic_auth_header: Option<String>,
    custom_headers: HashMap<String, String>,
    pass_through_status: bool,
    stream_response_sink: Option<std::sync::Arc<dyn MessagePublisher>>,
}

impl HttpPublisher {
    pub async fn new(config: &HttpConfig) -> anyhow::Result<Self> {
        Self::new_with_stream_response_sink(config, None).await
    }

    pub async fn new_with_stream_response_sink(
        config: &HttpConfig,
        stream_response_sink: Option<std::sync::Arc<dyn MessagePublisher>>,
    ) -> anyhow::Result<Self> {
        // Initialize TLS provider if TLS is configured for this endpoint.
        let batch_concurrency = config.batch_concurrency.unwrap_or(20).max(1);

        // Share one pooled client across publishers with the same client-level settings.
        let client = create_shared_http_client(config).await?;

        let url = config.tls.normalize_url(&config.url);

        let base_uri = url
            .parse::<hyper::Uri>()
            .map_err(|e| anyhow::anyhow!("Invalid configured URL '{}': {}", url, e))?;

        let method = config
            .method
            .as_deref()
            .map(|m| {
                hyper::Method::from_bytes(m.as_bytes())
                    .map_err(|_| anyhow::anyhow!("Invalid config.method: '{}'", m))
            })
            .transpose()?
            .unwrap_or(hyper::Method::POST);

        let request_timeout =
            std::time::Duration::from_millis(config.request_timeout_ms.unwrap_or(30000));

        let compression_threshold_bytes = config.compression_threshold_bytes.unwrap_or(1024);

        Ok(Self {
            client,
            url,
            base_uri,
            method,
            request_timeout,
            batch_concurrency,
            compression: config.publisher_compression(),
            compression_threshold_bytes,
            basic_auth_header: basic_auth_header_value(config.basic_auth.as_ref()),
            custom_headers: config.custom_headers.clone(),
            pass_through_status: config.pass_through_status,
            stream_response_sink,
        })
    }

    /// Core send that borrows `message`, so `send_batch` needn't clone every message just to
    /// keep it available for error reporting — this body only ever reads from it.
    async fn send_ref(&self, message: &CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(
            message_id = %format!("{:032x}", message.message_id),
            url = %self.url,
            "Sending HTTP request"
        );

        let method = message
            .metadata
            .get(HTTP_METHOD)
            .and_then(|m| hyper::Method::from_bytes(m.as_bytes()).ok())
            .unwrap_or_else(|| self.method.clone());

        let uri = if let Some(path) = message.metadata.get(HTTP_PATH) {
            let mut path_and_query = path.clone();
            if let Some(query) = message.metadata.get(HTTP_QUERY) {
                if !query.is_empty() {
                    path_and_query.push('?');
                    path_and_query.push_str(query);
                }
            }
            let mut builder = hyper::Uri::builder();
            if let Some(scheme) = self.base_uri.scheme() {
                builder = builder.scheme(scheme.clone());
            }
            if let Some(authority) = self.base_uri.authority() {
                builder = builder.authority(authority.clone());
            }
            builder
                .path_and_query(path_and_query)
                .build()
                .map_err(|e| {
                    PublisherError::NonRetryable(anyhow::anyhow!("Failed to build URI: {}", e))
                })?
        } else {
            self.base_uri.clone()
        };

        let mut request_builder = Request::builder().method(method).uri(uri);

        for (key, value) in &message.metadata {
            if key == HTTP_METHOD
                || key == HTTP_PATH
                || key == HTTP_QUERY
                || key == HTTP_VERSION
                || key == "tls_cipher_suite"
                || key == "tls_protocol_version"
                // Drop stale framing headers carried over as metadata: the body is re-framed
                // here (compress_if_needed sets Content-Encoding, hyper sets the length), so a
                // forwarded content-length/transfer-encoding/content-encoding would be wrong.
                // Mirrors make_response's filtering on the consumer side.
                || key.eq_ignore_ascii_case("content-length")
                || key.eq_ignore_ascii_case("transfer-encoding")
                || key.eq_ignore_ascii_case("content-encoding")
                || crate::canonical_message::is_source_metadata_key(key)
            {
                continue;
            }
            request_builder = request_builder.header(key, value);
        }

        // Only attach Basic auth when credentials are actually configured.
        if let Some(header_value) = self.basic_auth_header.as_deref() {
            request_builder = request_builder.header("Authorization", header_value);
        }

        // Add custom authentication headers
        for (header_name, header_value) in &self.custom_headers {
            request_builder = request_builder.header(header_name.as_str(), header_value.as_str());
        }

        // Advertise every coding we can decode so a peer may compress its response with whichever
        // it prefers — decompress_if_needed handles gzip/lz4/zstd regardless of the request-body
        // algorithm, and the raw hyper client negotiates nothing on its own. `None` leaves the
        // header off to keep responses uncompressed by default.
        if !matches!(self.compression, Compression::None) {
            request_builder = request_builder.header("Accept-Encoding", "gzip, lz4, zstd");
        }

        // Compress payload if enabled and beneficial
        let (payload_out, encoding) = compress_if_needed(
            message.payload.clone(),
            self.compression,
            self.compression_threshold_bytes,
        )
        .map_err(|e| {
            PublisherError::NonRetryable(anyhow::anyhow!("Failed to compress payload: {}", e))
        })?;

        if let Some(token) = encoding {
            request_builder = request_builder.header("Content-Encoding", token);
        }

        let body = http_body_util::Full::from(payload_out);
        let request = request_builder.body(body).map_err(|e| {
            PublisherError::NonRetryable(anyhow::anyhow!("Failed to build request: {}", e))
        })?;

        // Total wall-time budget for the whole send: header exchange plus body collection
        // must together fit within request_timeout, so body collection gets only the time
        // left after the response headers arrive.
        let request_deadline = std::time::Instant::now() + self.request_timeout;
        let future =
            tokio::time::timeout(self.request_timeout, self.client.pick().request(request));

        let response: hyper::Response<Incoming> = match future.await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                let error = anyhow::anyhow!("Failed to send HTTP request to {}: {}", self.url, e);
                return Err(PublisherError::Retryable(error));
            }
            Err(_) => {
                return Err(PublisherError::Retryable(anyhow::anyhow!(
                    "HTTP request timeout"
                )));
            }
        };

        let response_status = response.status();
        let stream_response_format = self.stream_response_sink.as_ref().and_then(|_| {
            super::stream::streaming_response_format_from_headers(response.headers())
        });
        let mut response_metadata = HashMap::with_capacity(response.headers().len() + 2);
        response_metadata.insert(
            HTTP_VERSION.to_string(),
            format!("{:?}", response.version()),
        );
        let mut content_encoding = None;
        for (key, value) in response.headers() {
            if let Ok(value_str) = value.to_str() {
                if key.as_str().eq_ignore_ascii_case("content-encoding") {
                    content_encoding = Some(value_str.to_string());
                }
                response_metadata.insert(key.as_str().to_string(), value_str.to_string());
            }
        }
        // Expose the response status alongside the headers/version — mirrors the consumer
        // side (which records http_method/http_path/…) so responses carry their status code.
        response_metadata.insert(
            HTTP_STATUS_CODE.to_string(),
            response_status.as_u16().to_string(),
        );

        if response_status.is_success() {
            if let (Some(stream_response_sink), Some(stream_response_format)) =
                (&self.stream_response_sink, stream_response_format)
            {
                if content_encoding.is_some() {
                    return Err(PublisherError::Retryable(anyhow::anyhow!(
                        "Compressed HTTP response streams cannot be published to stream_response_to"
                    )));
                }

                let correlation_id = message
                    .metadata
                    .get("correlation_id")
                    .cloned()
                    .unwrap_or_else(|| format!("{:032x}", message.message_id));
                match super::stream::publish_response_stream(
                    response.into_body(),
                    stream_response_sink.clone(),
                    response_metadata,
                    correlation_id,
                    stream_response_format,
                    self.request_timeout,
                )
                .await
                {
                    Ok(()) => {}
                    Err(PublishResponseStreamError::Partial(error)) => {
                        tracing::warn!(
                            "HTTP response stream terminated after partial publish: {}",
                            error
                        );
                    }
                    Err(PublishResponseStreamError::BeforePublish(error)) => return Err(error),
                }
                return Ok(Sent::Ack);
            }
        }

        // Use only the remaining budget for body collection — the request timeout was
        // already partly spent getting the response headers. Reusing the full timeout here
        // could double the total wall time a single send() call blocks the caller.
        let body_collect_timeout =
            request_deadline.saturating_duration_since(std::time::Instant::now());
        let response_bytes_raw = match tokio::time::timeout(
            body_collect_timeout,
            response.into_body().collect(),
        )
        .await
        {
            Ok(Ok(collected)) => collected.to_bytes(),
            Ok(Err(e)) => {
                return Err(PublisherError::Retryable(anyhow::anyhow!(
                    "Failed to read HTTP response body: {}",
                    e
                )))
            }
            Err(_) => {
                return Err(PublisherError::Retryable(anyhow::anyhow!(
                    "HTTP response body collection timeout"
                )))
            }
        };

        // Decompress response if needed
        let response_bytes = decompress_if_needed(response_bytes_raw, content_encoding.as_deref())
            .map_err(|e| {
                PublisherError::Retryable(anyhow::anyhow!("Failed to decompress response: {}", e))
            })?;

        if !response_status.is_success() && !self.pass_through_status {
            debug!(
                message_id = %format!("{:032x}", message.message_id),
                status = %response_status,
                "HTTP request failed"
            );
            let error = anyhow::anyhow!(
                "HTTP send request failed with status {}: {:?}",
                response_status,
                error_body_excerpt(&response_bytes)
            );

            if response_status.is_client_error() {
                // 408 Request Timeout and 429 Too Many Requests are transient: the same
                // request may well succeed on a later attempt, so let the route retry them.
                return match response_status.as_u16() {
                    408 | 429 => Err(PublisherError::Retryable(error)),
                    _ => Err(PublisherError::NonRetryable(error)),
                };
            } else if response_status.is_server_error() {
                match response_status.as_u16() {
                    501 | 505 => return Err(PublisherError::NonRetryable(error)),
                    _ => return Err(PublisherError::Retryable(error)),
                }
            }
            return Err(PublisherError::NonRetryable(error));
        }

        trace!(
            message_id = %format!("{:032x}", message.message_id),
            status = %response_status,
            "HTTP request succeeded"
        );

        let mut response_message =
            CanonicalMessage::new_bytes(response_bytes, Some(message.message_id));
        response_message.metadata = response_metadata;
        Ok(Sent::Response(response_message))
    }

    /// Wraps `send_ref` carrying the batch index, so `send_batch` can build its futures from a
    /// plain method call (an async closure here trips a higher-ranked-lifetime inference error).
    async fn send_ref_indexed(
        &self,
        idx: usize,
        message: &CanonicalMessage,
    ) -> (usize, Result<Sent, PublisherError>) {
        (idx, self.send_ref(message).await)
    }
}

#[async_trait]
impl MessagePublisher for HttpPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        self.send_ref(&message).await
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        use futures::StreamExt;

        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        if messages.len() == 1 {
            let message = messages.into_iter().next().expect("checked len");
            return match self.send_ref(&message).await {
                Ok(Sent::Ack) => Ok(SentBatch::Ack),
                Ok(Sent::Response(resp)) => Ok(SentBatch::Partial {
                    responses: Some(vec![resp]),
                    failed: Vec::new(),
                }),
                Err(e) => Ok(SentBatch::Partial {
                    responses: None,
                    failed: vec![(message, e)],
                }),
            };
        }

        trace!(
            count = messages.len(),
            url = %self.url,
            message_ids = ?LazyMessageIds(&messages),
            "Publishing batch of HTTP requests"
        );

        // Unordered so a slow request doesn't head-of-line-block harvesting completed ones;
        // responses carry their own message_id, so collection order is irrelevant. Futures are
        // materialized eagerly (only polled by `buffer_unordered`) to sidestep a higher-ranked
        // lifetime inference error from a lazy iterator of borrowing futures.
        let send_futures: Vec<_> = messages
            .iter()
            .enumerate()
            .map(|(idx, message)| self.send_ref_indexed(idx, message))
            .collect();
        let mut stream =
            futures::stream::iter(send_futures).buffer_unordered(self.batch_concurrency);

        let mut responses = Vec::new();
        let mut failed_indices: Vec<(usize, PublisherError)> = Vec::new();

        while let Some((idx, result)) = stream.next().await {
            match result {
                Ok(Sent::Response(resp)) => responses.push(resp),
                Ok(Sent::Ack) => {}
                Err(e) => failed_indices.push((idx, e)),
            }
        }
        drop(stream);

        // Reclaim owned messages only for the (rare) failures — avoids a per-message clone.
        let failed = if failed_indices.is_empty() {
            Vec::new()
        } else {
            let mut owned: Vec<Option<CanonicalMessage>> = messages.into_iter().map(Some).collect();
            failed_indices
                .into_iter()
                .map(|(idx, e)| {
                    (
                        owned[idx].take().expect("each failed index reclaimed once"),
                        e,
                    )
                })
                .collect()
        };

        if failed.is_empty() && responses.is_empty() {
            Ok(SentBatch::Ack)
        } else {
            Ok(SentBatch::Partial {
                responses: if responses.is_empty() {
                    None
                } else {
                    Some(responses)
                },
                failed,
            })
        }
    }

    async fn status(&self) -> crate::traits::EndpointStatus {
        crate::traits::EndpointStatus {
            healthy: true,
            target: self.url.clone(),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
