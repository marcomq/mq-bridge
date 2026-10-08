use crate::models::CookieJarMiddleware;
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received,
    ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[derive(Debug, Default, Clone)]
struct SessionState {
    /// `name -> (set sequence, value)`. Cookie names are chosen by the server, so a
    /// rotating name would grow the jar without bound; the sequence drives eviction
    /// once `max_cookies` is exceeded.
    cookies: HashMap<String, (u64, String)>,
    values: HashMap<String, String>,
    next_seq: u64,
}

impl SessionState {
    fn set_cookie(&mut self, name: String, value: String, max_cookies: usize) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.cookies.insert(name, (seq, value));
        while self.cookies.len() > max_cookies.max(1) {
            let oldest = self
                .cookies
                .iter()
                .min_by_key(|(_, (seq, _))| *seq)
                .map(|(name, _)| name.clone());
            match oldest {
                Some(name) => {
                    self.cookies.remove(&name);
                }
                None => break,
            }
        }
    }
}

type SessionStore = Arc<RwLock<SessionState>>;

static SHARED_SESSION_STORES: OnceLock<RwLock<HashMap<String, SessionStore>>> = OnceLock::new();

fn recover_read_lock<'a, T>(lock: &'a RwLock<T>, name: &str) -> RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|poisoned| {
        tracing::warn!(lock = name, "Recovering from poisoned read lock");
        poisoned.into_inner()
    })
}

fn recover_write_lock<'a, T>(lock: &'a RwLock<T>, name: &str) -> RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|poisoned| {
        tracing::warn!(lock = name, "Recovering from poisoned write lock");
        poisoned.into_inner()
    })
}

fn get_or_create_session_store(shared_scope: Option<&str>) -> SessionStore {
    match shared_scope {
        Some(scope) => {
            let registry = SHARED_SESSION_STORES.get_or_init(|| RwLock::new(HashMap::new()));
            if let Some(existing) = recover_read_lock(registry, "cookie_jar_registry")
                .get(scope)
                .cloned()
            {
                return existing;
            }

            let mut writer = recover_write_lock(registry, "cookie_jar_registry");
            writer
                .entry(scope.to_string())
                .or_insert_with(|| Arc::new(RwLock::new(SessionState::default())))
                .clone()
        }
        None => Arc::new(RwLock::new(SessionState::default())),
    }
}

fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
    header
        .split(';')
        .filter_map(|part| {
            let trimmed = part.trim();
            let (name, value) = trimmed.split_once('=')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// What a `Set-Cookie` header asks the jar to do.
enum SetCookie {
    Store(String, String),
    /// `Max-Age` at or below zero, the standard way to delete a cookie.
    Expire(String),
}

/// `Expires` is deliberately not honoured: parsing HTTP dates would need a date
/// dependency, and `max_cookies` already bounds the jar. `Max-Age` is unambiguous.
fn parse_set_cookie_header(header: &str) -> Option<SetCookie> {
    let mut parts = header.trim().split(';');
    let (name, value) = parts.next()?.trim().split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let expired = parts.any(|attr| {
        let Some((key, val)) = attr.trim().split_once('=') else {
            return false;
        };
        key.trim().eq_ignore_ascii_case("max-age")
            && val.trim().parse::<i64>().is_ok_and(|age| age <= 0)
    });

    Some(if expired {
        SetCookie::Expire(name.to_string())
    } else {
        SetCookie::Store(name.to_string(), value.trim().to_string())
    })
}

fn render_cookie_header(cookies: &HashMap<String, (u64, String)>) -> Option<String> {
    if cookies.is_empty() {
        return None;
    }

    let mut pairs: Vec<_> = cookies
        .iter()
        .map(|(k, (_, v))| format!("{k}={v}"))
        .collect();
    pairs.sort();
    Some(pairs.join("; "))
}

fn export_session_metadata(
    metadata: &mut HashMap<String, String>,
    store: &SessionStore,
    prefix: Option<&str>,
) {
    let Some(prefix) = prefix else {
        return;
    };

    let snapshot = recover_read_lock(store, "cookie_jar_session").clone();
    for (key, (_, value)) in snapshot.cookies {
        metadata.insert(format!("{prefix}cookie.{key}"), value);
    }
    for (key, value) in snapshot.values {
        metadata.insert(format!("{prefix}value.{key}"), value);
    }
}

fn capture_session_inputs(
    metadata: &HashMap<String, String>,
    store: &SessionStore,
    cookie_metadata_key: &str,
    set_cookie_metadata_key: &str,
    capture_metadata_keys: &[String],
    max_cookies: usize,
) {
    let mut state = recover_write_lock(store, "cookie_jar_session");

    if let Some(cookie_header) = metadata.get(cookie_metadata_key) {
        for (name, value) in parse_cookie_header(cookie_header) {
            state.set_cookie(name, value, max_cookies);
        }
    }

    if let Some(set_cookie_header) = metadata.get(set_cookie_metadata_key) {
        // One line per `Set-Cookie` header of the response.
        for line in set_cookie_header.lines() {
            match parse_set_cookie_header(line) {
                Some(SetCookie::Store(name, value)) => state.set_cookie(name, value, max_cookies),
                Some(SetCookie::Expire(name)) => {
                    state.cookies.remove(&name);
                }
                None => {}
            }
        }
    }

    for key in capture_metadata_keys {
        if let Some(value) = metadata.get(key) {
            state.values.insert(key.clone(), value.clone());
        }
    }
}

fn inject_session_metadata(
    metadata: &mut HashMap<String, String>,
    store: &SessionStore,
    cookie_metadata_key: &str,
    inject_metadata: &HashMap<String, String>,
    export_metadata_prefix: Option<&str>,
) {
    let snapshot = recover_read_lock(store, "cookie_jar_session").clone();

    if !metadata.contains_key(cookie_metadata_key) {
        if let Some(cookie_header) = render_cookie_header(&snapshot.cookies) {
            metadata.insert(cookie_metadata_key.to_string(), cookie_header);
        }
    }

    for (metadata_key, session_key) in inject_metadata {
        if metadata.contains_key(metadata_key) {
            continue;
        }
        // `export_metadata_prefix` reports names as `<prefix>cookie.<name>` /
        // `<prefix>value.<name>`, so accept that spelling here too — looking a name up
        // under the form it was just read back in used to silently inject nothing.
        let unprefixed = export_metadata_prefix
            .and_then(|prefix| session_key.strip_prefix(prefix))
            .unwrap_or(session_key);
        let value = match unprefixed.split_once('.') {
            Some(("cookie", name)) => snapshot.cookies.get(name).map(|(_, v)| v),
            Some(("value", name)) => snapshot.values.get(name),
            _ => None,
        }
        .or_else(|| snapshot.values.get(session_key))
        .or_else(|| snapshot.cookies.get(session_key).map(|(_, v)| v));

        if let Some(value) = value {
            metadata.insert(metadata_key.clone(), value.clone());
        }
    }
}

pub struct CookieJarConsumer {
    inner: Box<dyn MessageConsumer>,
    store: SessionStore,
    config: CookieJarMiddleware,
}

impl CookieJarConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &CookieJarMiddleware) -> Self {
        Self {
            inner,
            store: get_or_create_session_store(config.shared_scope.as_deref()),
            config: config.clone(),
        }
    }

    fn process_message(&self, message: &mut CanonicalMessage) {
        capture_session_inputs(
            &message.metadata,
            &self.store,
            &self.config.cookie_metadata_key,
            &self.config.set_cookie_metadata_key,
            &self.config.capture_metadata_keys,
            self.config.max_cookies,
        );
        export_session_metadata(
            &mut message.metadata,
            &self.store,
            self.config.export_metadata_prefix.as_deref(),
        );
    }
}

#[async_trait]
impl MessageConsumer for CookieJarConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.inner.set_exit_on_empty(exit_on_empty);
    }

    fn commit_requires_order(&self) -> bool {
        self.inner.commit_requires_order()
    }
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let mut received = self.inner.receive().await?;
        self.process_message(&mut received.message);
        Ok(received)
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut batch = self.inner.receive_batch(max_messages).await?;
        for message in &mut batch.messages {
            self.process_message(message);
        }
        Ok(batch)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct CookieJarPublisher {
    inner: Box<dyn MessagePublisher>,
    store: SessionStore,
    config: CookieJarMiddleware,
}

impl CookieJarPublisher {
    pub fn new(inner: Box<dyn MessagePublisher>, config: &CookieJarMiddleware) -> Self {
        Self {
            inner,
            store: get_or_create_session_store(config.shared_scope.as_deref()),
            config: config.clone(),
        }
    }

    fn prepare_message(&self, message: &mut CanonicalMessage) {
        capture_session_inputs(
            &message.metadata,
            &self.store,
            &self.config.cookie_metadata_key,
            &self.config.set_cookie_metadata_key,
            &self.config.capture_metadata_keys,
            self.config.max_cookies,
        );
        inject_session_metadata(
            &mut message.metadata,
            &self.store,
            &self.config.cookie_metadata_key,
            &self.config.inject_metadata,
            self.config.export_metadata_prefix.as_deref(),
        );
        export_session_metadata(
            &mut message.metadata,
            &self.store,
            self.config.export_metadata_prefix.as_deref(),
        );
    }

    fn process_response_message(&self, message: &mut CanonicalMessage) {
        capture_session_inputs(
            &message.metadata,
            &self.store,
            &self.config.cookie_metadata_key,
            &self.config.set_cookie_metadata_key,
            &self.config.capture_metadata_keys,
            self.config.max_cookies,
        );
        export_session_metadata(
            &mut message.metadata,
            &self.store,
            self.config.export_metadata_prefix.as_deref(),
        );
    }
}

#[async_trait]
impl MessagePublisher for CookieJarPublisher {
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_connect_hook()
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        self.inner.on_disconnect_hook()
    }

    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        self.prepare_message(&mut message);
        match self.inner.send(message).await? {
            Sent::Ack => Ok(Sent::Ack),
            Sent::Response(mut response) => {
                self.process_response_message(&mut response);
                Ok(Sent::Response(response))
            }
        }
    }

    async fn send_batch(
        &self,
        mut messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        for message in &mut messages {
            self.prepare_message(message);
        }

        match self.inner.send_batch(messages).await? {
            SentBatch::Ack => Ok(SentBatch::Ack),
            SentBatch::Partial {
                mut responses,
                failed,
            } => {
                if let Some(responses) = &mut responses {
                    for response in responses {
                        self.process_response_message(response);
                    }
                }
                Ok(SentBatch::Partial { responses, failed })
            }
        }
    }

    fn requires_ordered_publish(&self) -> bool {
        self.inner.requires_ordered_publish()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{BatchCommitFunc, MessagePublisher};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    fn ack_commit() -> BatchCommitFunc {
        Box::new(|_| Box::pin(async { Ok(()) }))
    }

    struct MockConsumer {
        messages: Option<Vec<CanonicalMessage>>,
    }

    #[async_trait]
    impl MessageConsumer for MockConsumer {
        async fn receive_batch(
            &mut self,
            _max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            Ok(ReceivedBatch {
                messages: self.messages.take().expect("batch already consumed"),
                commit: ack_commit(),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Clone)]
    struct RecordingPublisher {
        sent: Arc<Mutex<Vec<CanonicalMessage>>>,
        response_metadata: HashMap<String, String>,
    }

    #[async_trait]
    impl MessagePublisher for RecordingPublisher {
        async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
            self.sent.lock().unwrap().push(message.clone());
            let mut response = CanonicalMessage::from("ok");
            response.metadata = self.response_metadata.clone();
            Ok(Sent::Response(response))
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            self.sent.lock().unwrap().extend(messages);
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn max_age_zero_expires_a_cookie() {
        let mut state = SessionState::default();
        state.set_cookie("sid".to_string(), "abc".to_string(), 256);

        match parse_set_cookie_header("sid=abc; Path=/; Max-Age=0").unwrap() {
            SetCookie::Expire(name) => {
                state.cookies.remove(&name);
            }
            SetCookie::Store(..) => panic!("Max-Age=0 must expire the cookie"),
        }
        assert!(state.cookies.is_empty());

        assert!(matches!(
            parse_set_cookie_header("sid=abc; Max-Age=3600"),
            Some(SetCookie::Store(..))
        ));
    }

    #[tokio::test]
    async fn every_set_cookie_line_of_a_response_is_stored() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher = CookieJarPublisher::new(
            Box::new(RecordingPublisher {
                sent: sent.clone(),
                response_metadata: HashMap::from([(
                    "set-cookie".to_string(),
                    "sid=abc; Path=/; HttpOnly\ncsrf=xyz; Path=/\nold=1; Max-Age=0".to_string(),
                )]),
            }),
            &CookieJarMiddleware::default(),
        );

        for body in ["first", "second"] {
            publisher.send(CanonicalMessage::from(body)).await.unwrap();
        }

        let sent = sent.lock().unwrap();
        let cookie = sent[1].metadata.get("cookie").unwrap();
        let mut pairs: Vec<&str> = cookie.split("; ").collect();
        pairs.sort_unstable();
        assert_eq!(pairs, ["csrf=xyz", "sid=abc"]);
    }

    /// Cookie names come from the server, so a rotating name must not grow the jar.
    #[test]
    fn the_jar_is_capped_and_evicts_the_oldest() {
        let mut state = SessionState::default();
        for i in 0..10 {
            state.set_cookie(format!("rotating-{i}"), "v".to_string(), 3);
        }
        assert_eq!(state.cookies.len(), 3);
        for i in 7..10 {
            assert!(state.cookies.contains_key(&format!("rotating-{i}")));
        }
    }

    #[tokio::test]
    async fn test_cookie_jar_publisher_stores_set_cookie_and_injects_cookie_header() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let config = CookieJarMiddleware::default();
        let publisher = CookieJarPublisher::new(
            Box::new(RecordingPublisher {
                sent: sent.clone(),
                response_metadata: HashMap::from([(
                    "set-cookie".to_string(),
                    "session_id=abc123; Path=/; HttpOnly".to_string(),
                )]),
            }),
            &config,
        );

        publisher
            .send(CanonicalMessage::from("first"))
            .await
            .unwrap();
        publisher
            .send(CanonicalMessage::from("second"))
            .await
            .unwrap();

        let sent = sent.lock().unwrap();
        assert!(!sent[0].metadata.contains_key("cookie"));
        assert_eq!(
            sent[1].metadata.get("cookie").map(|s| s.as_str()),
            Some("session_id=abc123")
        );
    }

    #[tokio::test]
    async fn test_cookie_jar_shared_scope_can_move_values_from_consumer_to_publisher() {
        let scope = format!("shared-scope-{}", fast_uuid_v7::gen_id_string());
        let mut inbound = CanonicalMessage::from("input");
        inbound
            .metadata
            .insert("cookie".to_string(), "sid=xyz".to_string());
        inbound
            .metadata
            .insert("x-csrf-token".to_string(), "csrf123".to_string());

        let consumer_cfg = CookieJarMiddleware {
            shared_scope: Some(scope.clone()),
            capture_metadata_keys: vec!["x-csrf-token".to_string()],
            export_metadata_prefix: Some("session.".to_string()),
            ..Default::default()
        };
        let publisher_cfg = CookieJarMiddleware {
            shared_scope: Some(scope),
            inject_metadata: HashMap::from([(
                "x-forwarded-csrf".to_string(),
                "x-csrf-token".to_string(),
            )]),
            ..Default::default()
        };

        let mut consumer = CookieJarConsumer::new(
            Box::new(MockConsumer {
                messages: Some(vec![inbound]),
            }),
            &consumer_cfg,
        );
        let received = consumer.receive_batch(10).await.unwrap();
        assert_eq!(
            received.messages[0]
                .metadata
                .get("session.cookie.sid")
                .map(|s| s.as_str()),
            Some("xyz")
        );
        assert_eq!(
            received.messages[0]
                .metadata
                .get("session.value.x-csrf-token")
                .map(|s| s.as_str()),
            Some("csrf123")
        );

        let sent = Arc::new(Mutex::new(Vec::new()));
        let publisher = CookieJarPublisher::new(
            Box::new(RecordingPublisher {
                sent: sent.clone(),
                response_metadata: HashMap::new(),
            }),
            &publisher_cfg,
        );

        publisher.send(CanonicalMessage::from("out")).await.unwrap();

        let sent = sent.lock().unwrap();
        assert_eq!(
            sent[0].metadata.get("cookie").map(|s| s.as_str()),
            Some("sid=xyz")
        );
        assert_eq!(
            sent[0].metadata.get("x-forwarded-csrf").map(|s| s.as_str()),
            Some("csrf123")
        );
    }

    /// `inject_metadata` must also resolve a name held in the cookie store, not only one
    /// captured through `capture_metadata_keys`, and it must accept the same `cookie.<name>`
    /// spelling `export_metadata_prefix` reports back.
    #[tokio::test]
    async fn test_inject_metadata_resolves_a_captured_cookie_by_either_spelling() {
        let scope = format!("inject-scope-{}", fast_uuid_v7::gen_id_string());
        let mut inbound = CanonicalMessage::from("input");
        inbound.metadata.insert(
            "set-cookie".to_string(),
            "session=xyz789; Path=/".to_string(),
        );

        let mut consumer = CookieJarConsumer::new(
            Box::new(MockConsumer {
                messages: Some(vec![inbound]),
            }),
            &CookieJarMiddleware {
                shared_scope: Some(scope.clone()),
                ..Default::default()
            },
        );
        consumer.receive_batch(10).await.unwrap();

        for (session_key, export_prefix) in [
            ("session", None),
            ("cookie.session", None),
            ("session.cookie.session", Some("session.".to_string())),
        ] {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let publisher = CookieJarPublisher::new(
                Box::new(RecordingPublisher {
                    sent: sent.clone(),
                    response_metadata: HashMap::new(),
                }),
                &CookieJarMiddleware {
                    shared_scope: Some(scope.clone()),
                    inject_metadata: HashMap::from([(
                        "authorization".to_string(),
                        session_key.to_string(),
                    )]),
                    export_metadata_prefix: export_prefix,
                    ..Default::default()
                },
            );
            publisher.send(CanonicalMessage::from("out")).await.unwrap();

            let sent = sent.lock().unwrap();
            assert_eq!(
                sent[0].metadata.get("authorization").map(|s| s.as_str()),
                Some("xyz789"),
                "inject_metadata did not resolve {session_key:?}, got {:?}",
                sent[0].metadata
            );
        }
    }
}
