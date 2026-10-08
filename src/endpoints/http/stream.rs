// The `http` feature requires Rust 1.85 (see Cargo.toml), so slice `trim_ascii`
// (stable since 1.80) is always available here; the crate-wide 1.75 MSRV doesn't apply.
#![allow(clippy::incompatible_msrv)]

use crate::support::sse::{find_sse_event_end, parse_sse_event, ParsedSseEvent};
use crate::traits::{
    BoxFuture, CommitFunc, MessageDisposition, MessagePublisher, PublisherError, Sent,
};
use crate::CanonicalMessage;
use anyhow::anyhow;
use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::StreamBody;
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::{Response, StatusCode};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, trace, warn};

type BoxBody = http_body_util::combinators::BoxBody<Bytes, anyhow::Error>;
type HttpSourceMessage = (CanonicalMessage, CommitFunc);
type HttpStreamResponseTx = async_channel::Sender<Result<Frame<Bytes>, anyhow::Error>>;

pub(super) enum PublishResponseStreamError {
    BeforePublish(PublisherError),
    Partial(PublisherError),
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody {
    http_body_util::Full::new(chunk.into())
        .map_err(|_| anyhow!("Infallible"))
        .boxed()
}

fn streamed<S>(stream: S) -> BoxBody
where
    S: futures::Stream<Item = Result<Frame<Bytes>, anyhow::Error>> + Send + Sync + 'static,
{
    StreamBody::new(stream).boxed()
}

/// Rejects an NDJSON/SSE stream whose un-drained accumulation (a single record with no
/// record delimiter yet) has grown past the shared body cap, so a client cannot force
/// unbounded memory growth by streaming one endless "record".
fn bound_stream_buffer(buffer: &[u8]) -> anyhow::Result<()> {
    if buffer.len() as u64 > super::MAX_HTTP_BODY_BYTES {
        return Err(anyhow!(
            "stream record exceeds maximum buffered size of {} bytes",
            super::MAX_HTTP_BODY_BYTES
        ));
    }
    Ok(())
}

fn has_content_type_header(headers: &HashMap<String, String>) -> bool {
    headers
        .keys()
        .any(|key| key.eq_ignore_ascii_case("content-type"))
}

fn text_error_response(
    status: StatusCode,
    body: impl Into<Bytes>,
    accepts_text: bool,
    custom_headers: Option<&HashMap<String, String>>,
) -> Response<BoxBody> {
    let mut builder = Response::builder().status(status);

    if let Some(custom_headers) = custom_headers {
        for (header_name, header_value) in custom_headers {
            builder = builder.header(header_name.as_str(), header_value.as_str());
        }

        if accepts_text && !has_content_type_header(custom_headers) {
            builder = builder.header("content-type", "text/plain; charset=utf-8");
        }
    } else if accepts_text {
        builder = builder.header("content-type", "text/plain; charset=utf-8");
    }

    builder.body(full(body)).unwrap()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HttpStreamFormat {
    RawChunks,
    Ndjson,
    Sse,
}

impl HttpStreamFormat {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::RawChunks => "raw_chunks",
            Self::Ndjson => "ndjson",
            Self::Sse => "sse",
        }
    }
}

pub(super) fn header_contains_media(headers: &hyper::HeaderMap, name: &str, needle: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().ok().is_some_and(|raw| {
            raw.split(',')
                .filter_map(|item| item.split(';').next())
                .any(|media_type| media_type.trim().eq_ignore_ascii_case(needle))
        })
    })
}

pub(super) fn stream_request_format(headers: &hyper::HeaderMap) -> HttpStreamFormat {
    if header_contains_media(headers, "content-type", "text/event-stream") {
        HttpStreamFormat::Sse
    } else if header_contains_media(headers, "content-type", "application/x-ndjson")
        || header_contains_media(headers, "content-type", "application/jsonl")
    {
        HttpStreamFormat::Ndjson
    } else {
        HttpStreamFormat::RawChunks
    }
}

pub(super) fn stream_response_format(headers: &hyper::HeaderMap) -> HttpStreamFormat {
    if header_contains_media(headers, "accept", "text/event-stream") {
        HttpStreamFormat::Sse
    } else if header_contains_media(headers, "accept", "application/x-ndjson")
        || header_contains_media(headers, "accept", "application/jsonl")
    {
        HttpStreamFormat::Ndjson
    } else {
        HttpStreamFormat::RawChunks
    }
}

pub(super) fn stream_response_content_type(format: HttpStreamFormat) -> &'static str {
    match format {
        HttpStreamFormat::RawChunks => "application/octet-stream",
        HttpStreamFormat::Ndjson => "application/x-ndjson",
        HttpStreamFormat::Sse => "text/event-stream",
    }
}

pub(super) fn streaming_response_format_from_headers(
    headers: &hyper::HeaderMap,
) -> Option<HttpStreamFormat> {
    if header_contains_media(headers, "content-type", "text/event-stream") {
        Some(HttpStreamFormat::Sse)
    } else if header_contains_media(headers, "content-type", "application/x-ndjson")
        || header_contains_media(headers, "content-type", "application/jsonl")
    {
        Some(HttpStreamFormat::Ndjson)
    } else {
        None
    }
}

pub(super) fn format_stream_reply(format: HttpStreamFormat, message: &CanonicalMessage) -> Bytes {
    match format {
        HttpStreamFormat::RawChunks => message.payload.clone(),
        HttpStreamFormat::Ndjson => {
            let mut payload = Vec::with_capacity(message.payload.len() + 1);
            payload.extend_from_slice(&message.payload);
            payload.push(b'\n');
            Bytes::from(payload)
        }
        HttpStreamFormat::Sse => {
            let event_id = message
                .metadata
                .get("sse_id")
                .cloned()
                .unwrap_or_else(|| fast_uuid_v7::format_uuid(message.message_id).to_string());
            format_sse_frame(Some(&event_id), None, &message.payload)
        }
    }
}

pub(super) fn format_stream_error(format: HttpStreamFormat, error: &str) -> Bytes {
    match format {
        HttpStreamFormat::RawChunks => Bytes::copy_from_slice(error.as_bytes()),
        HttpStreamFormat::Ndjson => {
            let json = serde_json::json!({ "error": error }).to_string();
            Bytes::from(format!("{}\n", json))
        }
        HttpStreamFormat::Sse => format_sse_frame(None, Some("error"), error.as_bytes()),
    }
}

#[derive(Clone)]
pub(super) struct HttpReceiveStreamConfig {
    pub(super) tx: tokio::sync::mpsc::Sender<HttpSourceMessage>,
    pub(super) inline_publisher: Option<Arc<dyn MessagePublisher>>,
    pub(super) fire_and_forget: bool,
    pub(super) request_timeout: std::time::Duration,
    pub(super) custom_headers: HashMap<String, String>,
}

pub(super) async fn handle_streamable_request(
    mut body: Incoming,
    metadata: HashMap<String, String>,
    config: HttpReceiveStreamConfig,
    request_format: HttpStreamFormat,
    response_format: HttpStreamFormat,
    accepts_text: bool,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> anyhow::Result<Response<BoxBody>> {
    let response_tx = if config.fire_and_forget {
        None
    } else {
        Some(async_channel::bounded::<Result<Frame<Bytes>, anyhow::Error>>(16))
    };

    let stream_tx = response_tx.as_ref().map(|(tx, _)| tx.clone());
    let stream_rx = response_tx.map(|(_, rx)| rx);
    let custom_headers = config.custom_headers.clone();
    let fire_and_forget = config.fire_and_forget;

    tokio::spawn(async move {
        let result = receive_streamable_body(
            config,
            &mut body,
            metadata,
            request_format,
            response_format,
            stream_tx.clone(),
        )
        .await;

        if let Err(error) = result {
            warn!(error = %error, "HTTP streamable receive failed");
            if let Some(tx) = stream_tx {
                let _ = tx
                    .send(Ok(Frame::data(format_stream_error(
                        response_format,
                        &error.to_string(),
                    ))))
                    .await;
            }
        }

        drop(permit);
    });

    if fire_and_forget {
        let mut builder = Response::builder().status(StatusCode::ACCEPTED);
        for (header_name, header_value) in &custom_headers {
            builder = builder.header(header_name.as_str(), header_value.as_str());
        }
        return Ok(builder
            .body(full("Message stream accepted for processing"))
            .unwrap());
    }

    let mut builder = Response::builder().status(StatusCode::OK).header(
        "content-type",
        stream_response_content_type(response_format),
    );
    for (header_name, header_value) in &custom_headers {
        builder = builder.header(header_name.as_str(), header_value.as_str());
    }

    if let Some(rx) = stream_rx {
        Ok(builder.body(streamed(rx)).unwrap())
    } else {
        Ok(text_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "HTTP stream response channel was not created",
            accepts_text,
            Some(&custom_headers),
        ))
    }
}

async fn receive_streamable_body(
    config: HttpReceiveStreamConfig,
    body: &mut Incoming,
    metadata: HashMap<String, String>,
    request_format: HttpStreamFormat,
    response_format: HttpStreamFormat,
    response_tx: Option<HttpStreamResponseTx>,
) -> anyhow::Result<()> {
    let correlation_id = metadata
        .get("correlation_id")
        .cloned()
        .unwrap_or_else(fast_uuid_v7::gen_id_string);
    let dispatch = HttpStreamDispatch {
        tx: config.tx,
        inline_publisher: config.inline_publisher,
        request_timeout: config.request_timeout,
        correlation_id,
        request_format,
        response_format,
        response_tx,
    };
    let mut stream_index = 0usize;
    // Raw bytes accumulated across frames; records are framed on byte boundaries and
    // decoded only once complete, so a multi-byte UTF-8 char split across frames stays intact.
    let mut byte_buffer: Vec<u8> = Vec::new();

    loop {
        let next_frame = tokio::time::timeout(dispatch.request_timeout, body.frame()).await;
        let frame = match next_frame {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(error))) => return Err(anyhow!("Failed to read stream body: {}", error)),
            Ok(None) => break,
            Err(_) => return Err(anyhow!("Timed out reading stream body")),
        };

        let Some(data) = frame.data_ref() else {
            continue;
        };
        if data.is_empty() {
            continue;
        }

        match request_format {
            HttpStreamFormat::RawChunks => {
                send_stream_item(&dispatch, data.clone(), metadata.clone(), stream_index).await?;
                stream_index += 1;
            }
            HttpStreamFormat::Ndjson => {
                byte_buffer.extend_from_slice(data);
                stream_index = drain_ndjson_stream_items(
                    &dispatch,
                    &mut byte_buffer,
                    metadata.clone(),
                    stream_index,
                )
                .await?;
                bound_stream_buffer(&byte_buffer)?;
            }
            HttpStreamFormat::Sse => {
                byte_buffer.extend_from_slice(data);
                stream_index = drain_sse_stream_items(
                    &dispatch,
                    &mut byte_buffer,
                    metadata.clone(),
                    stream_index,
                )
                .await?;
                bound_stream_buffer(&byte_buffer)?;
            }
        }
    }

    match request_format {
        HttpStreamFormat::RawChunks => {}
        HttpStreamFormat::Ndjson => {
            // Trailing record with no closing newline; the whole remainder is one
            // complete record at EOF, so its raw bytes go through untouched.
            let tail = byte_buffer.trim_ascii();
            if !tail.is_empty() {
                send_stream_item(
                    &dispatch,
                    Bytes::copy_from_slice(tail),
                    metadata,
                    stream_index,
                )
                .await?;
            }
        }
        HttpStreamFormat::Sse => {
            if !byte_buffer.trim_ascii().is_empty() {
                let raw_event = String::from_utf8_lossy(&byte_buffer);
                if let Some(event) = parse_sse_event(&raw_event) {
                    send_sse_stream_item(&dispatch, event, metadata, stream_index).await?;
                }
            }
        }
    }

    Ok(())
}

async fn drain_ndjson_stream_items(
    dispatch: &HttpStreamDispatch,
    buffer: &mut Vec<u8>,
    metadata: HashMap<String, String>,
    mut stream_index: usize,
) -> anyhow::Result<usize> {
    while let Some(newline_pos) = buffer.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = buffer.drain(..=newline_pos).collect();
        line.pop(); // drop the '\n'
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        send_stream_item(dispatch, Bytes::from(line), metadata.clone(), stream_index).await?;
        stream_index += 1;
    }
    Ok(stream_index)
}

async fn drain_sse_stream_items(
    dispatch: &HttpStreamDispatch,
    buffer: &mut Vec<u8>,
    metadata: HashMap<String, String>,
    mut stream_index: usize,
) -> anyhow::Result<usize> {
    while let Some(event_end) = find_sse_event_end(buffer) {
        let terminator_len = if buffer[event_end..].starts_with(b"\r\n\r\n") {
            4
        } else {
            2
        };
        let event_bytes: Vec<u8> = buffer.drain(..event_end + terminator_len).collect();
        // The framed event is complete, so decoding here cannot split a multi-byte char.
        let raw_event = String::from_utf8_lossy(&event_bytes[..event_end]);
        let Some(event) = parse_sse_event(&raw_event) else {
            continue;
        };
        send_sse_stream_item(dispatch, event, metadata.clone(), stream_index).await?;
        stream_index += 1;
    }
    Ok(stream_index)
}

#[derive(Clone)]
struct HttpStreamDispatch {
    tx: tokio::sync::mpsc::Sender<HttpSourceMessage>,
    inline_publisher: Option<Arc<dyn MessagePublisher>>,
    request_timeout: std::time::Duration,
    correlation_id: String,
    request_format: HttpStreamFormat,
    response_format: HttpStreamFormat,
    response_tx: Option<HttpStreamResponseTx>,
}

async fn send_sse_stream_item(
    dispatch: &HttpStreamDispatch,
    event: ParsedSseEvent,
    mut metadata: HashMap<String, String>,
    stream_index: usize,
) -> anyhow::Result<()> {
    if let Some(event_id) = event.event_id {
        metadata.insert("sse_id".to_string(), event_id);
    }
    if let Some(event_name) = event.event_name {
        metadata.insert("sse_event".to_string(), event_name);
    }
    send_stream_item(dispatch, event.payload, metadata, stream_index).await
}

async fn send_stream_item(
    dispatch: &HttpStreamDispatch,
    payload: Bytes,
    mut metadata: HashMap<String, String>,
    stream_index: usize,
) -> anyhow::Result<()> {
    metadata.insert(
        "correlation_id".to_string(),
        dispatch.correlation_id.clone(),
    );
    metadata.insert(
        "http_stream_id".to_string(),
        dispatch.correlation_id.clone(),
    );
    metadata.insert("http_stream_index".to_string(), stream_index.to_string());
    metadata.insert(
        "http_stream_format".to_string(),
        dispatch.request_format.as_str().to_string(),
    );

    let mut message = CanonicalMessage::new_bytes(payload, None);
    trace!(
        message_id = format!("{:032x}", message.message_id),
        correlation_id = dispatch.correlation_id,
        stream_index,
        "Received HTTP stream item"
    );
    message.metadata = metadata;

    let response_tx = dispatch.response_tx.clone();
    let response_format = dispatch.response_format;
    let send_timeout = std::time::Duration::from_millis(2000).min(dispatch.request_timeout / 2);
    if let Some(inline_publisher) = dispatch.inline_publisher.as_ref() {
        let disposition =
            match tokio::time::timeout(dispatch.request_timeout, inline_publisher.send(message))
                .await
            {
                Ok(Ok(Sent::Response(response))) => MessageDisposition::Reply(response),
                Ok(Ok(Sent::Ack)) => MessageDisposition::Ack,
                Ok(Err(error)) => {
                    return Err(anyhow!("HTTP inline stream publisher failed: {}", error))
                }
                Err(_) => return Err(anyhow!("HTTP inline stream publisher timed out")),
            };

        return streamable_commit(disposition, response_tx, response_format, send_timeout).await;
    }

    let commit = Box::new(move |disposition: MessageDisposition| {
        streamable_commit(disposition, response_tx, response_format, send_timeout)
    });

    match tokio::time::timeout(send_timeout, dispatch.tx.send((message, commit))).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(anyhow!("Internal pipeline closed: {}", error)),
        Err(_) => Err(anyhow!("Server overloaded")),
    }
}

fn streamable_commit(
    disposition: MessageDisposition,
    response_tx: Option<HttpStreamResponseTx>,
    response_format: HttpStreamFormat,
    send_timeout: std::time::Duration,
) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move {
        let Some(response_tx) = response_tx else {
            return Ok(());
        };

        let frame = match disposition {
            MessageDisposition::Reply(message) => {
                Frame::data(format_stream_reply(response_format, &message))
            }
            MessageDisposition::Ack => return Ok(()),
            MessageDisposition::Nack => Frame::data(format_stream_error(
                response_format,
                "Message processing failed",
            )),
        };

        // A stalled reader must not pin this commit forever; a timeout is treated as a
        // closed stream.
        match tokio::time::timeout(send_timeout, response_tx.send(Ok(frame))).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => debug!("HTTP stream closed before response was sent"),
        }
        Ok(())
    })
}

fn format_sse_frame(event_id: Option<&str>, event: Option<&str>, payload: &[u8]) -> Bytes {
    let mut frame = String::new();
    if let Some(event_id) = event_id {
        if !event_id.contains('\n') && !event_id.contains('\r') {
            frame.push_str("id: ");
            frame.push_str(event_id);
            frame.push('\n');
        }
    }
    if let Some(event) = event {
        if !event.contains('\n') && !event.contains('\r') {
            frame.push_str("event: ");
            frame.push_str(event);
            frame.push('\n');
        }
    }

    let text = String::from_utf8_lossy(payload);
    if text.is_empty() {
        frame.push_str("data:\n");
    } else {
        for line in text.lines() {
            frame.push_str("data: ");
            frame.push_str(line);
            frame.push('\n');
        }
        if text.ends_with('\n') {
            frame.push_str("data:\n");
        }
    }
    frame.push('\n');
    Bytes::from(frame)
}

pub(super) async fn publish_response_stream(
    mut body: Incoming,
    sink: Arc<dyn MessagePublisher>,
    base_metadata: HashMap<String, String>,
    correlation_id: String,
    format: HttpStreamFormat,
    timeout: std::time::Duration,
) -> Result<(), PublishResponseStreamError> {
    let mut index = 0usize;
    // Raw bytes accumulated across frames; framed on byte boundaries and decoded only
    // once a record is complete, so a multi-byte UTF-8 char split across frames stays intact.
    let mut byte_buffer: Vec<u8> = Vec::new();
    let mut published_any = false;

    loop {
        let next_frame = tokio::time::timeout(timeout, body.frame()).await;
        let frame = match next_frame {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(error))) => {
                // Nothing published yet: leave the sink untouched so a retry starts a clean stream.
                if published_any
                    && publish_error_marker(
                        &sink,
                        &base_metadata,
                        &correlation_id,
                        format,
                        index,
                        &format!("Failed to read HTTP response stream: {}", error),
                    )
                    .await
                    .is_ok()
                {
                    let _ = publish_end_marker(
                        &sink,
                        &base_metadata,
                        &correlation_id,
                        format,
                        index + 1,
                    )
                    .await;
                }
                let err = PublisherError::Retryable(anyhow!(
                    "Failed to read HTTP response stream: {}",
                    error
                ));
                if published_any {
                    return Err(PublishResponseStreamError::Partial(err));
                }
                return Err(PublishResponseStreamError::BeforePublish(err));
            }
            Ok(None) => break,
            Err(_) => {
                if published_any
                    && publish_error_marker(
                        &sink,
                        &base_metadata,
                        &correlation_id,
                        format,
                        index,
                        "HTTP response stream timeout",
                    )
                    .await
                    .is_ok()
                {
                    let _ = publish_end_marker(
                        &sink,
                        &base_metadata,
                        &correlation_id,
                        format,
                        index + 1,
                    )
                    .await;
                }
                let err = PublisherError::Retryable(anyhow!("HTTP response stream timeout"));
                if published_any {
                    return Err(PublishResponseStreamError::Partial(err));
                }
                return Err(PublishResponseStreamError::BeforePublish(err));
            }
        };

        let Some(data) = frame.data_ref() else {
            continue;
        };
        if data.is_empty() {
            continue;
        }

        match format {
            HttpStreamFormat::RawChunks => {
                if let Err(error) = publish_stream_payload(
                    &sink,
                    data.clone(),
                    base_metadata.clone(),
                    &correlation_id,
                    format,
                    index,
                )
                .await
                {
                    if published_any {
                        let _ = publish_end_marker(
                            &sink,
                            &base_metadata,
                            &correlation_id,
                            format,
                            index,
                        )
                        .await;
                        return Err(PublishResponseStreamError::Partial(error));
                    }
                    return Err(PublishResponseStreamError::BeforePublish(error));
                }
                published_any = true;
                index += 1;
            }
            HttpStreamFormat::Ndjson => {
                byte_buffer.extend_from_slice(data);
                index = match drain_ndjson_response_items(
                    &sink,
                    &mut byte_buffer,
                    &base_metadata,
                    &correlation_id,
                    format,
                    index,
                )
                .await
                {
                    Ok(next_index) => next_index,
                    Err(error) => {
                        if published_any {
                            let _ = publish_end_marker(
                                &sink,
                                &base_metadata,
                                &correlation_id,
                                format,
                                index,
                            )
                            .await;
                            return Err(PublishResponseStreamError::Partial(error));
                        }
                        return Err(PublishResponseStreamError::BeforePublish(error));
                    }
                };
                if index > 0 {
                    published_any = true;
                }
            }
            HttpStreamFormat::Sse => {
                byte_buffer.extend_from_slice(data);
                index = match drain_sse_response_items(
                    &sink,
                    &mut byte_buffer,
                    &base_metadata,
                    &correlation_id,
                    format,
                    index,
                )
                .await
                {
                    Ok(next_index) => next_index,
                    Err(error) => {
                        if published_any {
                            let _ = publish_end_marker(
                                &sink,
                                &base_metadata,
                                &correlation_id,
                                format,
                                index,
                            )
                            .await;
                            return Err(PublishResponseStreamError::Partial(error));
                        }
                        return Err(PublishResponseStreamError::BeforePublish(error));
                    }
                };
                if index > 0 {
                    published_any = true;
                }
            }
        }
    }

    match format {
        HttpStreamFormat::RawChunks => {}
        HttpStreamFormat::Ndjson => {
            let tail = byte_buffer.trim_ascii();
            if !tail.is_empty() {
                if let Err(error) = publish_stream_payload(
                    &sink,
                    Bytes::copy_from_slice(tail),
                    base_metadata.clone(),
                    &correlation_id,
                    format,
                    index,
                )
                .await
                {
                    if published_any {
                        let _ = publish_end_marker(
                            &sink,
                            &base_metadata,
                            &correlation_id,
                            format,
                            index,
                        )
                        .await;
                        return Err(PublishResponseStreamError::Partial(error));
                    }
                    return Err(PublishResponseStreamError::BeforePublish(error));
                }
                published_any = true;
                index += 1;
            }
        }
        HttpStreamFormat::Sse => {
            if !byte_buffer.trim_ascii().is_empty() {
                let raw_event = String::from_utf8_lossy(&byte_buffer);
                if let Some(event) = parse_sse_event(&raw_event) {
                    if let Err(error) = publish_sse_response_item(
                        &sink,
                        event,
                        &base_metadata,
                        &correlation_id,
                        format,
                        index,
                    )
                    .await
                    {
                        if published_any {
                            let _ = publish_end_marker(
                                &sink,
                                &base_metadata,
                                &correlation_id,
                                format,
                                index,
                            )
                            .await;
                            return Err(PublishResponseStreamError::Partial(error));
                        }
                        return Err(PublishResponseStreamError::BeforePublish(error));
                    }
                    published_any = true;
                    index += 1;
                }
            }
        }
    }

    publish_end_marker(&sink, &base_metadata, &correlation_id, format, index)
        .await
        .map_err(|error| {
            if published_any {
                PublishResponseStreamError::Partial(error)
            } else {
                PublishResponseStreamError::BeforePublish(error)
            }
        })
}

async fn drain_ndjson_response_items(
    sink: &Arc<dyn MessagePublisher>,
    buffer: &mut Vec<u8>,
    base_metadata: &HashMap<String, String>,
    correlation_id: &str,
    format: HttpStreamFormat,
    mut index: usize,
) -> Result<usize, PublisherError> {
    while let Some(newline_pos) = buffer.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = buffer.drain(..=newline_pos).collect();
        line.pop(); // drop the '\n'
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        publish_stream_payload(
            sink,
            Bytes::from(line),
            base_metadata.clone(),
            correlation_id,
            format,
            index,
        )
        .await?;
        index += 1;
    }
    Ok(index)
}

async fn drain_sse_response_items(
    sink: &Arc<dyn MessagePublisher>,
    buffer: &mut Vec<u8>,
    base_metadata: &HashMap<String, String>,
    correlation_id: &str,
    format: HttpStreamFormat,
    mut index: usize,
) -> Result<usize, PublisherError> {
    while let Some(event_end) = find_sse_event_end(buffer) {
        let terminator_len = if buffer[event_end..].starts_with(b"\r\n\r\n") {
            4
        } else {
            2
        };
        let event_bytes: Vec<u8> = buffer.drain(..event_end + terminator_len).collect();
        // Complete framed event; safe to decode.
        let raw_event = String::from_utf8_lossy(&event_bytes[..event_end]);
        let Some(event) = parse_sse_event(&raw_event) else {
            continue;
        };
        publish_sse_response_item(sink, event, base_metadata, correlation_id, format, index)
            .await?;
        index += 1;
    }
    Ok(index)
}

async fn publish_sse_response_item(
    sink: &Arc<dyn MessagePublisher>,
    event: ParsedSseEvent,
    base_metadata: &HashMap<String, String>,
    correlation_id: &str,
    format: HttpStreamFormat,
    index: usize,
) -> Result<(), PublisherError> {
    let mut metadata = base_metadata.clone();
    if let Some(event_id) = event.event_id {
        metadata.insert("sse_id".to_string(), event_id);
    }
    if let Some(event_name) = event.event_name {
        metadata.insert("sse_event".to_string(), event_name);
    }
    publish_stream_payload(sink, event.payload, metadata, correlation_id, format, index).await
}

async fn publish_end_marker(
    sink: &Arc<dyn MessagePublisher>,
    base_metadata: &HashMap<String, String>,
    correlation_id: &str,
    format: HttpStreamFormat,
    index: usize,
) -> Result<(), PublisherError> {
    let mut metadata = base_metadata.clone();
    metadata.insert("http_stream_end".to_string(), "true".to_string());
    publish_stream_payload(sink, Bytes::new(), metadata, correlation_id, format, index).await
}

async fn publish_error_marker(
    sink: &Arc<dyn MessagePublisher>,
    base_metadata: &HashMap<String, String>,
    correlation_id: &str,
    format: HttpStreamFormat,
    index: usize,
    error: &str,
) -> Result<(), PublisherError> {
    let mut metadata = base_metadata.clone();
    metadata.insert("http_stream_error".to_string(), "true".to_string());
    publish_stream_payload(
        sink,
        Bytes::copy_from_slice(error.as_bytes()),
        metadata,
        correlation_id,
        format,
        index,
    )
    .await
}

async fn publish_stream_payload(
    sink: &Arc<dyn MessagePublisher>,
    payload: Bytes,
    mut metadata: HashMap<String, String>,
    correlation_id: &str,
    format: HttpStreamFormat,
    index: usize,
) -> Result<(), PublisherError> {
    metadata.insert("correlation_id".to_string(), correlation_id.to_string());
    metadata.insert("http_stream_id".to_string(), correlation_id.to_string());
    metadata.insert("http_stream_index".to_string(), index.to_string());
    metadata.insert(
        "http_stream_format".to_string(),
        format.as_str().to_string(),
    );
    metadata
        .entry("http_stream_end".to_string())
        .or_insert_with(|| "false".to_string());

    let mut message = CanonicalMessage::new_bytes(payload, None);
    message.metadata = metadata;

    match sink.send(message).await? {
        Sent::Ack | Sent::Response(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_formats_are_selected_from_headers() {
        let sse_headers = hyper::HeaderMap::from_iter([(
            hyper::header::CONTENT_TYPE,
            hyper::header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
        )]);
        assert_eq!(stream_request_format(&sse_headers), HttpStreamFormat::Sse);

        let ndjson_headers = hyper::HeaderMap::from_iter([(
            hyper::header::ACCEPT,
            hyper::header::HeaderValue::from_static("application/json, application/x-ndjson"),
        )]);
        assert_eq!(
            stream_response_format(&ndjson_headers),
            HttpStreamFormat::Ndjson
        );
    }

    #[test]
    fn test_format_stream_reply_as_sse_frame() {
        let message =
            CanonicalMessage::from_vec("hello\nworld").with_metadata_kv("sse_id", "reply-1");

        let frame = format_stream_reply(HttpStreamFormat::Sse, &message);
        let frame = std::str::from_utf8(&frame).expect("utf8 frame");

        assert!(frame.contains("id: reply-1\n"));
        assert!(frame.contains("data: hello\n"));
        assert!(frame.contains("data: world\n"));
        assert!(frame.ends_with("\n\n"));
    }
}
