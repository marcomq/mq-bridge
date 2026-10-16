//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Connection pool of the HTTP publisher, on hyper's stable `client::conn` API.
//!
//! One pool serves one origin. TCP, TLS and ALPN come from the `hyper-rustls` connector; idle
//! HTTP/1 connections are reused most-recently-used first, and an HTTP/2 connection negotiated
//! by ALPN is shared by all requests.

use super::*;
use hyper::client::conn::{http1, http2};
use hyper_util::client::legacy::connect::Connection;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

type Connector = hyper_rustls::HttpsConnector<HttpConnector>;
type Body = http_body_util::Full<Bytes>;
type Idle = VecDeque<(http1::SendRequest<Body>, Instant)>;

pub(super) struct HttpPool {
    connector: Connector,
    origin: hyper::Uri,
    host: hyper::header::HeaderValue,
    idle_timeout: Duration,
    idle: Arc<Mutex<Idle>>,
    h2: Mutex<Option<http2::SendRequest<Body>>>,
}

/// An HTTP/1 connection on loan; give it back with [`Lease::release`] once the response body
/// has been read. Dropping it instead closes the connection.
pub(super) struct Lease<'a> {
    pool: &'a HttpPool,
    sender: Option<http1::SendRequest<Body>>,
}

impl Lease<'_> {
    pub(super) fn release(mut self) {
        if let Some(sender) = self.sender.take() {
            self.pool.checkin(sender);
        }
    }
}

enum Sender {
    H1(http1::SendRequest<Body>),
    H2(http2::SendRequest<Body>),
}

impl HttpPool {
    pub(super) fn new(config: &HttpConfig, origin: &hyper::Uri) -> anyhow::Result<Self> {
        let tls_client_config = crate::support::tls::client_config(&config.tls)
            .context("Failed to create rustls client config")?;
        let mut http_connector = HttpConnector::new();
        http_connector.enforce_http(false);
        http_connector.set_nodelay(true);
        if let Some(keepalive) = config.tcp_keepalive_ms {
            http_connector.set_keepalive(Some(Duration::from_millis(keepalive)));
        }
        let connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls_client_config)
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http_connector);

        let authority = origin
            .authority()
            .ok_or_else(|| anyhow!("URL has no host: {origin}"))?;
        let default_port = match origin.scheme_str() {
            Some("https") => 443,
            _ => 80,
        };
        let host = match authority.port_u16() {
            Some(port) if port != default_port => authority.as_str(),
            _ => authority.host(),
        };
        let idle_timeout = Duration::from_millis(config.pool_idle_timeout_ms.unwrap_or(90_000));
        let idle = Arc::new(Mutex::new(Idle::new()));
        spawn_idle_reaper(Arc::downgrade(&idle), idle_timeout);
        Ok(Self {
            connector,
            origin: origin.clone(),
            host: hyper::header::HeaderValue::from_str(host)?,
            idle_timeout,
            idle,
            h2: Mutex::new(None),
        })
    }

    /// Sends `request` (absolute URI) on a pooled or new connection. A request that a reused
    /// connection could not take, because the peer had closed it, is sent again on a new one.
    pub(super) async fn send(
        &self,
        request: Request<Body>,
    ) -> anyhow::Result<(Response<Incoming>, Lease<'_>)> {
        // Boxed: the connect state is ~8 KB and would otherwise be copied with every send future.
        let (sender, reused) = match self.checkout().await {
            Some(sender) => (sender, true),
            None => (Box::pin(self.connect()).await?, false),
        };
        match self.try_send(sender, request).await {
            Ok(sent) => Ok(sent),
            Err((mut error, Some(request))) if reused => {
                debug!("pooled HTTP connection was closed, retrying on a new one: {error}");
                let sender = Box::pin(self.connect()).await?;
                error = match self.try_send(sender, *request).await {
                    Ok(sent) => return Ok(sent),
                    Err((error, _)) => error,
                };
                Err(error.into())
            }
            Err((error, _)) => Err(error.into()),
        }
    }

    async fn try_send(
        &self,
        sender: Sender,
        request: Request<Body>,
    ) -> Result<(Response<Incoming>, Lease<'_>), (hyper::Error, Option<Box<Request<Body>>>)> {
        let unsent = |mut e: hyper::client::conn::TrySendError<Request<Body>>| {
            let request = e.take_message().map(Box::new);
            (e.into_error(), request)
        };
        match sender {
            Sender::H1(mut sender) => {
                let response = sender
                    .try_send_request(self.origin_form(request))
                    .await
                    .map_err(unsent)?;
                let lease = Lease {
                    pool: self,
                    sender: Some(sender),
                };
                Ok((response, lease))
            }
            Sender::H2(mut sender) => {
                let response = sender.try_send_request(request).await.map_err(unsent)?;
                let lease = Lease {
                    pool: self,
                    sender: None,
                };
                Ok((response, lease))
            }
        }
    }

    /// HTTP/1 sends the path in the request line and the authority in `Host`.
    fn origin_form(&self, request: Request<Body>) -> Request<Body> {
        let (mut parts, body) = request.into_parts();
        let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
        if let Ok(uri) = hyper::Uri::try_from(path) {
            parts.uri = uri;
        }
        parts
            .headers
            .entry(hyper::header::HOST)
            .or_insert_with(|| self.host.clone());
        Request::from_parts(parts, body)
    }

    async fn checkout(&self) -> Option<Sender> {
        {
            let mut h2 = self.h2.lock().expect("http pool lock");
            match h2.as_ref() {
                Some(sender) if !sender.is_closed() => return Some(Sender::H2(sender.clone())),
                Some(_) => *h2 = None,
                None => {}
            }
        }
        loop {
            let (mut sender, last_used) = self.idle.lock().expect("http pool lock").pop_back()?;
            if last_used.elapsed() < self.idle_timeout && sender.ready().await.is_ok() {
                return Some(Sender::H1(sender));
            }
        }
    }

    fn checkin(&self, sender: http1::SendRequest<Body>) {
        if sender.is_closed() {
            return;
        }
        let mut idle = self.idle.lock().expect("http pool lock");
        drop_expired(&mut idle, self.idle_timeout);
        idle.push_back((sender, Instant::now()));
    }

    async fn connect(&self) -> anyhow::Result<Sender> {
        use tower_service::Service;
        let mut connector = self.connector.clone();
        std::future::poll_fn(|cx| connector.poll_ready(cx))
            .await
            .map_err(|e| anyhow!("connect: {e}"))?;
        let io = connector
            .call(self.origin.clone())
            .await
            .map_err(|e| anyhow!("connect: {e}"))?;
        if io.connected().is_negotiated_h2() {
            let (sender, connection) = http2::handshake(TokioExecutor::new(), io).await?;
            spawn_connection(connection);
            *self.h2.lock().expect("http pool lock") = Some(sender.clone());
            Ok(Sender::H2(sender))
        } else {
            let (sender, connection) = http1::handshake(io).await?;
            spawn_connection(connection);
            Ok(Sender::H1(sender))
        }
    }
}

fn spawn_connection(connection: impl Future<Output = hyper::Result<()>> + Send + 'static) {
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            debug!("HTTP client connection closed with an error: {e}");
        }
    });
}

/// Idle connections are oldest first, so the expired ones are at the front.
fn drop_expired(idle: &mut Idle, idle_timeout: Duration) {
    while idle
        .front()
        .is_some_and(|(_, last_used)| last_used.elapsed() >= idle_timeout)
    {
        idle.pop_front();
    }
}

/// Closes connections that stay idle past `idle_timeout` when no request comes to do it; ends
/// with the pool.
fn spawn_idle_reaper(idle: std::sync::Weak<Mutex<Idle>>, idle_timeout: Duration) {
    let period = idle_timeout.max(Duration::from_millis(100));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(period).await;
            let Some(idle) = idle.upgrade() else { break };
            drop_expired(&mut idle.lock().expect("http pool lock"), idle_timeout);
        }
    });
}
