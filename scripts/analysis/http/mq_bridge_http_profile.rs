// A standalone profiling binary, not library code: bailing out of a bad run with a
// non-zero status is the point. `clippy::exit` is denied workspace-wide because the
// library is embedded in host processes (Python/Node bindings) that it must never kill.
#![allow(clippy::exit)]

#[cfg(feature = "http")]
use mq_bridge::endpoints::create_publisher_from_route;
#[cfg(feature = "http")]
use mq_bridge::models::{Endpoint, EndpointType, HttpConfig};
#[cfg(feature = "http")]
use mq_bridge::traits::{MessageConsumer, MessageDisposition, MessagePublisher, Sent};
#[cfg(feature = "http")]
use mq_bridge::CanonicalMessage;
#[cfg(feature = "http")]
use mq_bridge::Route;
#[cfg(feature = "http")]
use std::collections::HashMap;
#[cfg(feature = "http")]
use std::convert::Infallible;
#[cfg(feature = "http")]
use std::net::SocketAddr;
#[cfg(feature = "http")]
use std::sync::Arc;

#[cfg(feature = "http")]
use bytes::Bytes;
#[cfg(feature = "http")]
use http_body_util::BodyExt;
#[cfg(feature = "http")]
use hyper::body::Incoming;
#[cfg(feature = "http")]
use hyper::service::service_fn;
#[cfg(feature = "http")]
use hyper::{Request, Response};
#[cfg(feature = "http")]
use hyper_util::client::legacy::connect::HttpConnector;
#[cfg(feature = "http")]
use hyper_util::rt::{TokioExecutor, TokioIo};
#[cfg(feature = "http")]
use hyper_util::server::conn::auto::Builder as AutoBuilder;

#[cfg(not(feature = "http"))]
fn main() {
    eprintln!("mq_bridge_http_profile requires --features http");
    std::process::exit(2);
}

#[cfg(feature = "http")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = ProfileArgs::parse();

    if let Some(url) = args.client_url.clone() {
        return run_rust_client(args, url).await;
    }

    if matches!(
        args.mode,
        ProfileMode::PublisherDirect | ProfileMode::PublisherRoute
    ) {
        return run_publisher_load(args).await;
    }

    if matches!(
        args.mode,
        ProfileMode::DirectConsumer
            | ProfileMode::DirectConsumerAck
            | ProfileMode::DirectConsumerFireForget
    ) {
        return run_direct_consumer(args).await;
    }

    if matches!(
        args.mode,
        ProfileMode::InlineResponse | ProfileMode::InlineBodyOnly
    ) {
        return run_inline_response(args).await;
    }

    if !matches!(
        args.mode,
        ProfileMode::Route | ProfileMode::RouteFireForget | ProfileMode::RouteHandler
    ) {
        return run_standalone_server(args).await;
    }

    let mut http = HttpConfig::new(format!("127.0.0.1:{}", args.port));
    http.path = Some(args.path.clone());
    http.method = Some("POST".to_string());
    http.internal_buffer_size = Some(args.internal_buffer_size);
    http.concurrency_limit = Some(args.concurrency_limit);
    http.request_timeout_ms = Some(args.request_timeout_ms);
    http.workers = Some(args.workers);
    http.fire_and_forget = args.mode == ProfileMode::RouteFireForget;

    let output = if args.mode == ProfileMode::RouteHandler {
        let mut endpoint = Endpoint::new_response();
        endpoint.handler = Some(Arc::new(|msg: CanonicalMessage| async move {
            Ok(mq_bridge::traits::Handled::Publish(msg))
        }));
        endpoint
    } else {
        Endpoint::new_response()
    };

    let route = Route::new(Endpoint::new(EndpointType::Http(http)), output)
        .with_concurrency(args.route_concurrency)
        .with_commit_concurrency_limit(args.commit_concurrency_limit)
        .with_batch_size(args.batch_size);

    let handle = route.run("http_profile").await?;
    println!(
        "READY http://127.0.0.1:{}{} duration_s={}",
        args.port, args.path, args.duration_s
    );

    tokio::time::sleep(std::time::Duration::from_secs(args.duration_s)).await;
    handle.stop().await;
    let _ = handle.join().await;
    Ok(())
}

#[cfg(feature = "http")]
struct ProfileArgs {
    mode: ProfileMode,
    port: u16,
    path: String,
    duration_s: u64,
    route_concurrency: usize,
    commit_concurrency_limit: usize,
    batch_size: usize,
    internal_buffer_size: usize,
    concurrency_limit: usize,
    request_timeout_ms: u64,
    workers: usize,
    client_url: Option<String>,
    clients: usize,
    header_count: usize,
    expected_body: ExpectedBody,
}

#[cfg(feature = "http")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProfileMode {
    Route,
    RouteFireForget,
    RouteHandler,
    Immediate,
    Metadata,
    ChannelAck,
    WorkerLocalAck,
    DirectConsumer,
    DirectConsumerAck,
    DirectConsumerFireForget,
    InlineResponse,
    InlineBodyOnly,
    /// Drive `HttpPublisher` directly with `--clients` concurrent senders (isolates the
    /// publisher from the route pipeline; compare against `--client-url` raw client).
    PublisherDirect,
    /// Drive a full memory-source -> HTTP-output route (the real publish path).
    PublisherRoute,
}

#[cfg(feature = "http")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedBody {
    Payload,
    Ok,
    MessageProcessed,
    MessageAccepted,
}

#[cfg(feature = "http")]
impl ProfileArgs {
    fn parse() -> Self {
        let mut args = Self {
            mode: ProfileMode::Route,
            port: 18080,
            path: "/bench".to_string(),
            duration_s: 45,
            route_concurrency: 8,
            commit_concurrency_limit: 1,
            batch_size: 128,
            internal_buffer_size: 65_536,
            concurrency_limit: 512,
            request_timeout_ms: 30_000,
            workers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            client_url: None,
            clients: 8,
            header_count: 0,
            expected_body: ExpectedBody::Payload,
        };

        let mut iter = std::env::args().skip(1);
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--mode" => args.mode = parse_mode(&next_value(&mut iter, &flag)),
                "--client-url" => args.client_url = Some(next_value(&mut iter, &flag)),
                "--clients" => args.clients = parse_next(&mut iter, &flag),
                "--header-count" => args.header_count = parse_next(&mut iter, &flag),
                "--expected-body" => {
                    args.expected_body = parse_expected_body(&next_value(&mut iter, &flag))
                }
                "--port" => args.port = parse_next(&mut iter, &flag),
                "--path" => args.path = next_value(&mut iter, &flag),
                "--duration-s" => args.duration_s = parse_next(&mut iter, &flag),
                "--route-concurrency" => args.route_concurrency = parse_next(&mut iter, &flag),
                "--commit-concurrency-limit" => {
                    args.commit_concurrency_limit = parse_next(&mut iter, &flag)
                }
                "--batch-size" => args.batch_size = parse_next(&mut iter, &flag),
                "--internal-buffer-size" => {
                    args.internal_buffer_size = parse_next(&mut iter, &flag)
                }
                "--concurrency-limit" => args.concurrency_limit = parse_next(&mut iter, &flag),
                "--request-timeout-ms" => args.request_timeout_ms = parse_next(&mut iter, &flag),
                "--workers" => args.workers = parse_next(&mut iter, &flag),
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    print_help();
                    std::process::exit(2);
                }
            }
        }

        if !args.path.starts_with('/') {
            args.path.insert(0, '/');
        }
        args
    }
}

#[cfg(feature = "http")]
fn parse_expected_body(value: &str) -> ExpectedBody {
    match value {
        "payload" => ExpectedBody::Payload,
        "ok" => ExpectedBody::Ok,
        "message-processed" | "message_processed" => ExpectedBody::MessageProcessed,
        "message-accepted" | "message_accepted" => ExpectedBody::MessageAccepted,
        other => {
            eprintln!("invalid --expected-body: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

#[cfg(feature = "http")]
fn parse_mode(value: &str) -> ProfileMode {
    match value {
        "route" => ProfileMode::Route,
        "route-fire-forget" | "route_fire_forget" => ProfileMode::RouteFireForget,
        "route-handler" | "route_handler" => ProfileMode::RouteHandler,
        "immediate" => ProfileMode::Immediate,
        "metadata" => ProfileMode::Metadata,
        "channel-ack" | "channel_ack" => ProfileMode::ChannelAck,
        "worker-local-ack" | "worker_local_ack" => ProfileMode::WorkerLocalAck,
        "direct-consumer" | "direct_consumer" => ProfileMode::DirectConsumer,
        "direct-consumer-ack" | "direct_consumer_ack" => ProfileMode::DirectConsumerAck,
        "direct-consumer-fire-forget" | "direct_consumer_fire_forget" => {
            ProfileMode::DirectConsumerFireForget
        }
        "inline-response" | "inline_response" => ProfileMode::InlineResponse,
        "inline-body-only" | "inline_body_only" => ProfileMode::InlineBodyOnly,
        "publisher-direct" | "publisher_direct" => ProfileMode::PublisherDirect,
        "publisher-route" | "publisher_route" => ProfileMode::PublisherRoute,
        other => {
            eprintln!("invalid --mode: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

#[cfg(feature = "http")]
fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> String {
    iter.next().unwrap_or_else(|| {
        eprintln!("missing value for {flag}");
        std::process::exit(2);
    })
}

#[cfg(feature = "http")]
fn parse_next<T>(iter: &mut impl Iterator<Item = String>, flag: &str) -> T
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = next_value(iter, flag);
    value.parse().unwrap_or_else(|err| {
        eprintln!("invalid value for {flag}: {value} ({err})");
        std::process::exit(2);
    })
}

#[cfg(feature = "http")]
fn print_help() {
    eprintln!(
        "Usage: mq_bridge_http_profile [--mode MODE] [--port PORT] [--duration-s SECONDS] [--path PATH]\n\
         Options:\n\
           --mode MODE                 route | route-fire-forget | route-handler | immediate | metadata | channel-ack | worker-local-ack | direct-consumer | direct-consumer-ack | direct-consumer-fire-forget | inline-response | inline-body-only | publisher-direct | publisher-route (default route)\n\
           publisher-direct            Drive HttpPublisher directly with --clients senders against an in-process sink\n\
           publisher-route             Drive a memory->http route (uses --route-concurrency / --batch-size)\n\
           --client-url URL            run Rust load client instead of server\n\
           --clients N                 Rust load client concurrency (default 8)\n\
           --header-count N            Add N synthetic request headers in Rust load client (default 0)\n\
           --expected-body BODY        payload | ok | message-processed | message-accepted (default payload)\n\
           --route-concurrency N       default 8\n\
           --commit-concurrency-limit N default 1\n\
           --batch-size N              default 128\n\
           --internal-buffer-size N    default 65536\n\
           --concurrency-limit N       default 512\n\
           --request-timeout-ms N      default 30000\n\
           --workers N                 default available parallelism"
    );
}

#[cfg(feature = "http")]
const PAYLOAD: &[u8] = br#"{"value":0}"#;

#[cfg(feature = "http")]
type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

#[cfg(feature = "http")]
type AckMessage = (
    Bytes,
    HashMap<String, String>,
    tokio::sync::oneshot::Sender<()>,
);

#[cfg(feature = "http")]
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody {
    http_body_util::Full::new(chunk.into()).boxed()
}

#[cfg(feature = "http")]
async fn run_standalone_server(args: ProfileArgs) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{}", args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    let channel_tx = if args.mode == ProfileMode::ChannelAck {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AckMessage>(args.internal_buffer_size);
        tokio::spawn(async move {
            while let Some((_payload, _metadata, ack)) = rx.recv().await {
                let _ = ack.send(());
            }
        });
        Some(tx)
    } else {
        None
    };
    let mode = args.mode;
    let path = Arc::new(args.path.clone());
    let duration_s = args.duration_s;
    let buffer_size = args.internal_buffer_size;

    let shutdown = tokio::time::sleep(std::time::Duration::from_secs(duration_s));
    tokio::pin!(shutdown);

    println!(
        "READY http://{}{} duration_s={} mode={:?}",
        bound_addr, args.path, duration_s, mode
    );

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let io = TokioIo::new(stream);
                // WorkerLocalAck gives every connection its own channel + drain task, so
                // there is no shared single-receiver contention (variant B in the ladder).
                // ChannelAck reuses the one process-wide channel (variant C). The
                // per-request work is otherwise identical, isolating the global funnel.
                let tx = if mode == ProfileMode::WorkerLocalAck {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<AckMessage>(buffer_size);
                    tokio::spawn(async move {
                        while let Some((_payload, _metadata, ack)) = rx.recv().await {
                            let _ = ack.send(());
                        }
                    });
                    Some(tx)
                } else {
                    channel_tx.clone()
                };
                let path = path.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        profile_request(mode, path.clone(), tx.clone(), req)
                    });
                    let builder = AutoBuilder::new(TokioExecutor::new());
                    let conn = builder.serve_connection(io, service);
                    if let Err(err) = conn.await {
                        eprintln!("standalone HTTP connection error: {err}");
                    }
                });
            }
        }
    }

    Ok(())
}

#[cfg(feature = "http")]
async fn run_direct_consumer(args: ProfileArgs) -> anyhow::Result<()> {
    let mut http = HttpConfig::new(format!("127.0.0.1:{}", args.port));
    http.path = Some(args.path.clone());
    http.method = Some("POST".to_string());
    http.internal_buffer_size = Some(args.internal_buffer_size);
    http.concurrency_limit = Some(args.concurrency_limit);
    http.request_timeout_ms = Some(args.request_timeout_ms);
    http.workers = Some(args.workers);
    http.fire_and_forget = args.mode == ProfileMode::DirectConsumerFireForget;

    let mut consumer = mq_bridge::endpoints::http::HttpConsumer::new(&http).await?;
    let bound_addr = consumer
        .bound_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| format!("127.0.0.1:{}", args.port));
    println!(
        "READY http://{}{} duration_s={} mode={:?}",
        bound_addr, args.path, args.duration_s, args.mode
    );

    let batch_size = args.batch_size;
    let mode = args.mode;
    let worker = tokio::spawn(async move {
        loop {
            let batch = consumer.receive_batch(batch_size).await?;
            let dispositions = match mode {
                ProfileMode::DirectConsumer => batch
                    .messages
                    .into_iter()
                    .map(MessageDisposition::Reply)
                    .collect::<Vec<_>>(),
                ProfileMode::DirectConsumerAck => {
                    vec![MessageDisposition::Ack; batch.messages.len()]
                }
                ProfileMode::DirectConsumerFireForget => {
                    vec![MessageDisposition::Ack; batch.messages.len()]
                }
                _ => unreachable!("direct consumer runner called with non-direct mode"),
            };
            (batch.commit)(dispositions).await?;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });

    tokio::time::sleep(std::time::Duration::from_secs(args.duration_s)).await;
    worker.abort();
    let _ = worker.await;
    Ok(())
}

#[cfg(feature = "http")]
async fn run_inline_response(args: ProfileArgs) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{}", args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    let publisher =
        create_publisher_from_route("http_profile_inline", &Endpoint::new_response()).await?;
    let path = Arc::new(args.path.clone());
    let duration_s = args.duration_s;

    let shutdown = tokio::time::sleep(std::time::Duration::from_secs(duration_s));
    tokio::pin!(shutdown);

    println!(
        "READY http://{}{} duration_s={} mode={:?}",
        bound_addr, args.path, duration_s, args.mode
    );

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let io = TokioIo::new(stream);
                let path = path.clone();
                let publisher = publisher.clone();
                let mode = args.mode;
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        inline_response_request(mode, path.clone(), publisher.clone(), req)
                    });
                    let builder = AutoBuilder::new(TokioExecutor::new());
                    let conn = builder.serve_connection(io, service);
                    if let Err(err) = conn.await {
                        eprintln!("inline response HTTP connection error: {err}");
                    }
                });
            }
        }
    }

    Ok(())
}

#[cfg(feature = "http")]
async fn profile_request(
    mode: ProfileMode,
    expected_path: Arc<String>,
    tx: Option<tokio::sync::mpsc::Sender<AckMessage>>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody>, Infallible> {
    if req.uri().path() != expected_path.as_str() {
        return Ok(Response::builder()
            .status(404)
            .body(full("not found"))
            .unwrap());
    }

    let metadata = if matches!(
        mode,
        ProfileMode::Metadata | ProfileMode::ChannelAck | ProfileMode::WorkerLocalAck
    ) {
        build_metadata(&req)
    } else {
        HashMap::new()
    };

    let payload = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return Ok(Response::builder()
                .status(500)
                .body(full("body error"))
                .unwrap());
        }
    };

    if matches!(mode, ProfileMode::ChannelAck | ProfileMode::WorkerLocalAck) {
        let Some(tx) = tx else {
            return Ok(Response::builder()
                .status(500)
                .body(full("missing channel"))
                .unwrap());
        };
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if tx.send((payload, metadata, ack_tx)).await.is_err() {
            return Ok(Response::builder()
                .status(503)
                .body(full("channel closed"))
                .unwrap());
        }
        if ack_rx.await.is_err() {
            return Ok(Response::builder()
                .status(503)
                .body(full("ack closed"))
                .unwrap());
        }
    }

    Ok(Response::new(full("ok")))
}

#[cfg(feature = "http")]
async fn inline_response_request(
    mode: ProfileMode,
    expected_path: Arc<String>,
    publisher: Arc<dyn MessagePublisher>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody>, Infallible> {
    if req.uri().path() != expected_path.as_str() {
        return Ok(Response::builder()
            .status(404)
            .body(full("not found"))
            .unwrap());
    }

    let mut metadata = build_metadata(&req);
    metadata.insert("reply_to".to_string(), "__http_inline__".to_string());

    let payload = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return Ok(Response::builder()
                .status(500)
                .body(full("body error"))
                .unwrap());
        }
    };

    let mut message = CanonicalMessage::new_bytes(payload, None);
    message.metadata = metadata;

    let response = match publisher.send(message).await {
        Ok(Sent::Response(message)) => {
            let mut builder = Response::builder().status(200);
            if mode == ProfileMode::InlineResponse {
                for (key, value) in &message.metadata {
                    if key.eq_ignore_ascii_case("http_status_code") {
                        continue;
                    }
                    builder = builder.header(key.as_str(), value.as_str());
                }
            }
            builder.body(full(message.payload)).unwrap()
        }
        Ok(Sent::Ack) => Response::builder()
            .status(500)
            .body(full("missing response"))
            .unwrap(),
        Err(err) => Response::builder()
            .status(500)
            .body(full(err.to_string()))
            .unwrap(),
    };

    Ok(response)
}

#[cfg(feature = "http")]
fn build_metadata(req: &Request<Incoming>) -> HashMap<String, String> {
    let mut metadata = HashMap::with_capacity(req.headers().len() + 4);
    metadata.insert("http_method".to_string(), req.method().as_str().to_string());
    metadata.insert("http_path".to_string(), req.uri().path().to_string());
    metadata.insert(
        "http_query".to_string(),
        req.uri().query().unwrap_or("").to_string(),
    );
    metadata.insert("http_version".to_string(), format!("{:?}", req.version()));

    for (key, value) in req.headers() {
        if let Ok(value) = value.to_str() {
            metadata.insert(key.as_str().to_string(), value.to_string());
        }
    }

    metadata
}

/// Spawns a minimal in-process HTTP sink that counts every request it receives.
/// Used as the target for the publisher-under-test modes.
#[cfg(feature = "http")]
async fn spawn_sink_server(
    counter: Arc<std::sync::atomic::AtomicU64>,
) -> anyhow::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let io = TokioIo::new(stream);
            let counter = counter.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let counter = counter.clone();
                    async move {
                        let _ = req.into_body().collect().await;
                        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Ok::<_, Infallible>(Response::new(full("ok")))
                    }
                });
                let builder = AutoBuilder::new(TokioExecutor::new());
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });
    Ok(addr)
}

/// Measures throughput of mq-bridge's own `HttpPublisher`, either driven directly
/// (`publisher-direct`) or through a full memory->http route (`publisher-route`).
/// Requests are counted at an in-process sink, so the number is delivered req/s.
#[cfg(feature = "http")]
async fn run_publisher_load(args: ProfileArgs) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    // Install the process-level rustls provider (the HTTP publisher builds a client TLS
    // config even for plain http:// targets).
    #[cfg(feature = "rustls-aws-lc")]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    #[cfg(all(feature = "rustls-ring", not(feature = "rustls-aws-lc")))]
    let _ = rustls::crypto::ring::default_provider().install_default();

    let counter = Arc::new(AtomicU64::new(0));
    let addr = spawn_sink_server(counter.clone()).await?;
    let url = format!("http://{}{}", addr, args.path);

    let mut http = HttpConfig::new(url);
    http.method = Some("POST".to_string());
    http.request_timeout_ms = Some(args.request_timeout_ms);

    println!(
        "READY sink=http://{}{} mode={:?} duration_s={} clients={} route_concurrency={} batch_size={}",
        addr, args.path, args.mode, args.duration_s, args.clients, args.route_concurrency, args.batch_size
    );

    let stop = Arc::new(AtomicBool::new(false));
    let mut route_handle = None;
    let mut tasks: Vec<tokio::task::JoinHandle<anyhow::Result<()>>> = Vec::new();

    match args.mode {
        ProfileMode::PublisherDirect => {
            // MQB_PER_SENDER_CLIENT=1 gives each sender its own HttpPublisher (own pooled
            // client) instead of one shared client — isolates shared connection-pool
            // contention, which is the suspect for send not scaling with cores.
            let per_sender = std::env::var("MQB_PER_SENDER_CLIENT").as_deref() == Ok("1");
            let shared_publisher: Option<Arc<dyn MessagePublisher>> = if per_sender {
                None
            } else {
                Some(Arc::new(
                    mq_bridge::endpoints::http::HttpPublisher::new(&http).await?,
                ))
            };
            for _ in 0..args.clients {
                let stop = stop.clone();
                let publisher: Arc<dyn MessagePublisher> = match &shared_publisher {
                    Some(p) => p.clone(),
                    None => {
                        let mut http = http.clone();
                        http.shared = Some(false);
                        Arc::new(mq_bridge::endpoints::http::HttpPublisher::new(&http).await?)
                    }
                };
                tasks.push(tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let msg = CanonicalMessage::new_bytes(Bytes::from_static(PAYLOAD), None);
                        publisher
                            .send(msg)
                            .await
                            .map_err(|e| anyhow::anyhow!("producer send failed: {e}"))?;
                    }
                    Ok(())
                }));
            }
        }
        ProfileMode::PublisherRoute => {
            let topic = format!("pub_route_{}", std::process::id());
            // Small item bound so the feeder is paced by the consumer (backpressure).
            let input = Endpoint::new_memory(&topic, 256);
            let channel = input.channel().expect("memory endpoint has a channel");
            let route = Route::new(input.clone(), Endpoint::new(EndpointType::Http(http)))
                .with_concurrency(args.route_concurrency)
                .with_commit_concurrency_limit(args.commit_concurrency_limit)
                .with_batch_size(args.batch_size);
            route_handle = Some(route.run("pub_route").await?);

            let batch_size = args.batch_size;
            let stop_feeder = stop.clone();
            tasks.push(tokio::spawn(async move {
                while !stop_feeder.load(Ordering::Relaxed) {
                    let batch: Vec<CanonicalMessage> = (0..batch_size)
                        .map(|_| CanonicalMessage::new_bytes(Bytes::from_static(PAYLOAD), None))
                        .collect();
                    channel
                        .fill_messages(batch)
                        .await
                        .map_err(|e| anyhow::anyhow!("route feeder fill_messages failed: {e}"))?;
                }
                channel.close();
                Ok(())
            }));
        }
        _ => unreachable!("run_publisher_load called with non-publisher mode"),
    }

    // Warm up, then measure a steady-state window.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let c0 = counter.load(Ordering::Relaxed);
    let t0 = std::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_secs(args.duration_s)).await;
    let delivered = counter.load(Ordering::Relaxed) - c0;
    let elapsed = t0.elapsed().as_secs_f64();

    let rate = delivered as f64 / elapsed;
    println!(
        "Publisher load ({:?}): {} requests in {:.2}s ({:.0} req/s)",
        args.mode, delivered, elapsed, rate
    );

    // Tear down. Abort producer tasks first (a route feeder may be parked in a full
    // channel), then stop the route. A producer that exited with an error *before* this
    // shutdown means the measured rate is bogus, so reject the whole run in that case;
    // a task cancelled by the abort is the normal shutdown path.
    stop.store(true, Ordering::Relaxed);
    let mut producer_error: Option<anyhow::Error> = None;
    for task in tasks {
        task.abort();
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if producer_error.is_none() {
                    producer_error = Some(e);
                }
            }
            Err(e) if e.is_cancelled() => {}
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
    if let Some(handle) = route_handle {
        handle.stop().await;
        let _ = handle.join().await;
    }
    if let Some(e) = producer_error {
        return Err(e);
    }
    Ok(())
}

#[cfg(feature = "http")]
async fn run_rust_client(args: ProfileArgs, url: String) -> anyhow::Result<()> {
    let uri: hyper::Uri = url.parse()?;
    let expected = match args.expected_body {
        ExpectedBody::Payload => Bytes::from_static(PAYLOAD),
        ExpectedBody::Ok => Bytes::from_static(b"ok"),
        ExpectedBody::MessageProcessed => Bytes::from_static(b"Message processed"),
        ExpectedBody::MessageAccepted => Bytes::from_static(b"Message accepted for processing"),
    };
    let duration = std::time::Duration::from_secs(args.duration_s);
    let stop_at = std::time::Instant::now() + duration;
    let header_count = args.header_count;
    // MQB_RAW_CONN=spawn|inline: bypass the pooled legacy client with one hyper conn per task.
    let raw_conn = std::env::var("MQB_RAW_CONN").ok();
    // MQB_CLIENT=legacy-shared|reqwest|pool: one client shared by every task, like a publisher.
    let shared = match std::env::var("MQB_CLIENT").ok() {
        Some(mode) => Some(Arc::new(SharedClient::new(&mode, &uri)?)),
        None => None,
    };

    let mut tasks = Vec::with_capacity(args.clients);
    for _ in 0..args.clients {
        let uri = uri.clone();
        let expected = expected.clone();
        if let Some(shared) = shared.clone() {
            tasks.push(tokio::spawn(shared_client_task(
                shared, uri, expected, stop_at,
            )));
            continue;
        }
        if let Some(raw_mode) = raw_conn.clone() {
            tasks.push(tokio::spawn(raw_conn_client(
                raw_mode, uri, expected, stop_at,
            )));
            continue;
        }
        tasks.push(tokio::spawn(async move {
            let mut connector = HttpConnector::new();
            connector.set_nodelay(true);
            let client =
                hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(connector);
            let mut count = 0usize;

            while std::time::Instant::now() < stop_at {
                let mut request_builder = Request::builder()
                    .method(hyper::Method::POST)
                    .uri(uri.clone())
                    .header("content-type", "application/json")
                    .header("accept", "application/octet-stream");
                for index in 0..header_count {
                    request_builder =
                        request_builder.header(format!("x-bench-{index}"), "bench-value");
                }
                let request = request_builder.body(http_body_util::Full::<Bytes>::new(
                    Bytes::from_static(PAYLOAD),
                ))?;
                let response = client.request(request).await?;
                if !response.status().is_success() {
                    anyhow::bail!("HTTP {}", response.status());
                }
                let body = response.into_body().collect().await?.to_bytes();
                if body != expected {
                    anyhow::bail!("unexpected body: {:?}", body);
                }
                count += 1;
            }

            anyhow::Ok(count)
        }));
    }

    let started_at = std::time::Instant::now();
    let mut requests = 0usize;
    for task in tasks {
        requests += task.await??;
    }
    let elapsed = started_at.elapsed();
    let rate = requests as f64 / elapsed.as_secs_f64();
    println!(
        "Rust load: {} requests in {:.2}s ({:.0} req/s)",
        requests,
        elapsed.as_secs_f64(),
        rate
    );
    Ok(())
}

#[cfg(feature = "http")]
type RawSender = hyper::client::conn::http1::SendRequest<http_body_util::Full<Bytes>>;

/// Minimal pool on the stable `conn::http1` API: idle senders in a `Mutex<Vec>`.
#[cfg(feature = "http")]
struct RawPool {
    authority: String,
    idle: std::sync::Mutex<Vec<RawSender>>,
}

#[cfg(feature = "http")]
impl RawPool {
    async fn checkout(&self) -> anyhow::Result<RawSender> {
        loop {
            let Some(sender) = self.idle.lock().expect("pool lock").pop() else {
                break;
            };
            if !sender.is_closed() {
                return Ok(sender);
            }
        }
        let stream = tokio::net::TcpStream::connect(&self.authority).await?;
        stream.set_nodelay(true)?;
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn);
        Ok(sender)
    }

    fn checkin(&self, sender: RawSender) {
        if !sender.is_closed() {
            self.idle.lock().expect("pool lock").push(sender);
        }
    }
}

#[cfg(feature = "http")]
#[allow(clippy::large_enum_variant)]
enum SharedClient {
    Legacy(hyper_util::client::legacy::Client<HttpConnector, http_body_util::Full<Bytes>>),
    #[cfg(feature = "http-bulk")]
    Reqwest(reqwest::Client),
    Pool(RawPool),
}

#[cfg(feature = "http")]
impl SharedClient {
    fn new(mode: &str, uri: &hyper::Uri) -> anyhow::Result<Self> {
        Ok(match mode {
            "legacy-shared" => {
                let mut connector = HttpConnector::new();
                connector.set_nodelay(true);
                Self::Legacy(
                    hyper_util::client::legacy::Client::builder(TokioExecutor::new())
                        .build(connector),
                )
            }
            #[cfg(feature = "http-bulk")]
            "reqwest" => Self::Reqwest(reqwest::Client::builder().tcp_nodelay(true).build()?),
            "pool" => Self::Pool(RawPool {
                authority: format!(
                    "{}:{}",
                    uri.host().unwrap_or("127.0.0.1"),
                    uri.port_u16().unwrap_or(80)
                ),
                idle: std::sync::Mutex::new(Vec::new()),
            }),
            other => anyhow::bail!("unknown MQB_CLIENT: {other}"),
        })
    }
}

#[cfg(feature = "http")]
async fn shared_client_task(
    shared: Arc<SharedClient>,
    uri: hyper::Uri,
    expected: Bytes,
    stop_at: std::time::Instant,
) -> anyhow::Result<usize> {
    let mut count = 0usize;
    while std::time::Instant::now() < stop_at {
        let body = match shared.as_ref() {
            SharedClient::Legacy(client) => {
                let request = Request::builder()
                    .method(hyper::Method::POST)
                    .uri(uri.clone())
                    .header("content-type", "application/json")
                    .body(http_body_util::Full::new(Bytes::from_static(PAYLOAD)))?;
                let response = client.request(request).await?;
                anyhow::ensure!(response.status().is_success(), "HTTP {}", response.status());
                response.into_body().collect().await?.to_bytes()
            }
            #[cfg(feature = "http-bulk")]
            SharedClient::Reqwest(client) => {
                let response = client
                    .post(uri.to_string())
                    .header("content-type", "application/json")
                    .body(Bytes::from_static(PAYLOAD))
                    .send()
                    .await?;
                anyhow::ensure!(response.status().is_success(), "HTTP {}", response.status());
                response.bytes().await?
            }
            SharedClient::Pool(pool) => {
                let mut sender = pool.checkout().await?;
                let request = Request::builder()
                    .method(hyper::Method::POST)
                    .uri(uri.path())
                    .header("host", pool.authority.as_str())
                    .header("content-type", "application/json")
                    .body(http_body_util::Full::new(Bytes::from_static(PAYLOAD)))?;
                let response = sender.send_request(request).await?;
                anyhow::ensure!(response.status().is_success(), "HTTP {}", response.status());
                let body = response.into_body().collect().await?.to_bytes();
                pool.checkin(sender);
                body
            }
        };
        anyhow::ensure!(body == expected, "unexpected body: {:?}", body);
        count += 1;
    }
    Ok(count)
}

/// One `hyper::client::conn::http1` connection; `inline` polls it in this task, `spawn` in its own.
#[cfg(feature = "http")]
async fn raw_conn_client(
    mode: String,
    uri: hyper::Uri,
    expected: Bytes,
    stop_at: std::time::Instant,
) -> anyhow::Result<usize> {
    let host = uri.host().unwrap_or("127.0.0.1").to_string();
    let authority = format!("{}:{}", host, uri.port_u16().unwrap_or(80));
    let path = uri.path().to_string();
    let stream = tokio::net::TcpStream::connect(&authority).await?;
    stream.set_nodelay(true)?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake::<_, http_body_util::Full<Bytes>>(TokioIo::new(
            stream,
        ))
        .await?;
    let mut conn: std::pin::Pin<Box<dyn std::future::Future<Output = hyper::Result<()>> + Send>> =
        if mode == "spawn" {
            tokio::spawn(conn);
            Box::pin(futures::future::pending())
        } else {
            Box::pin(conn)
        };
    let mut count = 0usize;
    while std::time::Instant::now() < stop_at {
        let request = Request::builder()
            .method(hyper::Method::POST)
            .uri(path.as_str())
            .header("host", authority.as_str())
            .header("content-type", "application/json")
            .body(http_body_util::Full::new(Bytes::from_static(PAYLOAD)))?;
        let exchange = async {
            let response = sender.send_request(request).await?;
            if !response.status().is_success() {
                anyhow::bail!("HTTP {}", response.status());
            }
            let body = response.into_body().collect().await?.to_bytes();
            if body != expected {
                anyhow::bail!("unexpected body: {:?}", body);
            }
            anyhow::Ok(())
        };
        tokio::select! {
            biased;
            result = exchange => result?,
            result = &mut conn => {
                result?;
                anyhow::bail!("connection closed");
            }
        }
        count += 1;
    }
    Ok(count)
}
