use crate::traits::{Handled, Handler, HandlerError};
use crate::{CanonicalMessage, MessageContext};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

/// A handler that dispatches messages to other handlers based on a metadata field (e.g., "type").
///
/// # Example
/// ```rust
/// use mq_bridge::type_handler::TypeHandler;
/// use mq_bridge::{CanonicalMessage, Handled};
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct MyCommand { id: String }
///
/// let handler = TypeHandler::new()
///     .add("my_command", |cmd: MyCommand| async move {
///         println!("Received command: {}", cmd.id);
///         Ok(Handled::Ack)
///     });
/// ```
#[derive(Clone)]
pub struct TypeHandler {
    pub(crate) handlers: HashMap<String, Arc<dyn Handler>>,
    pub(crate) type_key: String, // will be the key in msg metadata, default is "kind"
    pub(crate) fallback: Option<Arc<dyn Handler>>,
}

/// A helper trait to allow registering handlers with or without context.
pub trait IntoTypedHandler<T, Args>: Send + Sync + 'static {
    type Future: Future<Output = Result<Handled, HandlerError>> + Send + 'static;
    fn call(&self, msg: T, ctx: MessageContext) -> Self::Future;
}

impl<F, Fut, T> IntoTypedHandler<T, (T,)> for F
where
    T: DeserializeOwned + Send + Sync + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Handled, HandlerError>> + Send + 'static,
{
    type Future = Fut;
    fn call(&self, msg: T, _ctx: MessageContext) -> Self::Future {
        (self)(msg)
    }
}

impl<F, Fut, T> IntoTypedHandler<T, (T, MessageContext)> for F
where
    T: DeserializeOwned + Send + Sync + 'static,
    F: Fn(T, MessageContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Handled, HandlerError>> + Send + 'static,
{
    type Future = Fut;
    fn call(&self, msg: T, ctx: MessageContext) -> Self::Future {
        (self)(msg, ctx)
    }
}

impl TypeHandler {
    /// Creates a new TypeHandler that looks for the specified key in message metadata to determine the message type.
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            type_key: "kind".into(),
            fallback: None,
        }
    }

    /// Registers a generic handler for a specific type name.
    pub fn add_handler(mut self, type_name: &str, handler: impl Handler + 'static) -> Self {
        self.handlers
            .insert(type_name.to_string(), Arc::new(handler));
        self
    }

    /// Sets a fallback handler to be used when no type match is found.
    pub fn with_fallback(mut self, handler: Arc<dyn Handler>) -> Self {
        self.fallback = Some(handler);
        self
    }

    #[doc(hidden)]
    pub fn add_simple<T, F, Fut>(self, type_name: &str, handler: F) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Handled, HandlerError>> + Send + 'static,
    {
        self.add(type_name, handler)
    }

    /// Registers a typed handler function.
    ///
    /// The handler can accept either:
    /// - `fn(T) -> Future<Output = Result<Handled, HandlerError>>`
    /// - `fn(T, MessageContext) -> Future<Output = Result<Handled, HandlerError>>`
    pub fn add<T, H, Args>(mut self, type_name: &str, handler: H) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        H: IntoTypedHandler<T, Args>,
        Args: Send + Sync + 'static,
    {
        let handler = Arc::new(handler);
        let wrapper = move |msg: CanonicalMessage| {
            let handler = handler.clone();
            async move {
                let data = msg.parse::<T>().map_err(|e| {
                    HandlerError::NonRetryable(anyhow::anyhow!("Deserialization failed: {}", e))
                })?;
                let ctx = MessageContext::from(msg);
                handler.call(data, ctx).await
            }
        };
        self.handlers
            .insert(type_name.to_string(), Arc::new(wrapper));
        self
    }
}

#[async_trait]
impl Handler for TypeHandler {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        if let Some(type_val) = msg.metadata.get(&self.type_key) {
            if let Some(handler) = self.handlers.get(type_val) {
                return handler.handle(msg).await;
            }
        }

        if let Some(fallback) = &self.fallback {
            return fallback.handle(msg).await;
        }

        Err(HandlerError::NonRetryable(anyhow::anyhow!(
            "No handler registered for type: '{:?}' and no fallback provided",
            msg.metadata.get(&self.type_key)
        )))
    }

    fn register_handler(
        &self,
        type_name: &str,
        handler: Arc<dyn Handler>,
    ) -> Option<Arc<dyn Handler>> {
        let mut th = self.clone();
        th.handlers.insert(type_name.to_string(), handler);
        Some(Arc::new(th))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct TestMsg {
        val: String,
    }

    #[tokio::test]
    async fn test_typed_handler_dispatch() {
        let handler = TypeHandler::new().add("test_a", |msg: TestMsg| async move {
            assert_eq!(msg.val, "hello");
            Ok(Handled::Ack)
        });

        let msg = CanonicalMessage::from_type(&TestMsg {
            val: "hello".into(),
        })
        .unwrap()
        .with_metadata(HashMap::from([("kind".to_string(), "test_a".to_string())]));

        let res = handler.handle(msg).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_typed_handler_with_context() {
        use crate::MessageContext;
        let handler =
            TypeHandler::new().add("test_ctx", |msg: TestMsg, ctx: MessageContext| async move {
                assert_eq!(msg.val, "hello");
                assert_eq!(ctx.metadata.get("meta").map(|s| s.as_str()), Some("data"));
                Ok(Handled::Ack)
            });

        let msg = CanonicalMessage::from_type(&TestMsg {
            val: "hello".into(),
        })
        .unwrap()
        .with_metadata(HashMap::from([
            ("kind".to_string(), "test_ctx".to_string()),
            ("meta".to_string(), "data".to_string()),
        ]));

        let res = handler.handle(msg).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_typed_handler_no_match_error() {
        let handler = TypeHandler::new();
        let msg = CanonicalMessage::new(b"{}".to_vec(), None)
            .with_metadata(HashMap::from([("kind".to_string(), "unknown".to_string())]));

        let res = handler.handle(msg).await;
        assert!(res.is_err());
        match res.unwrap_err() {
            HandlerError::NonRetryable(e) => {
                assert!(e.to_string().contains("No handler registered"))
            }
            _ => panic!("Expected NonRetryable error"),
        }
    }

    #[tokio::test]
    async fn test_typed_handler_fallback_ack() {
        let fallback = Arc::new(|_: CanonicalMessage| async { Ok(Handled::Ack) });
        let handler = TypeHandler::new().with_fallback(fallback);

        let msg = CanonicalMessage::new(b"{}".to_vec(), None)
            .with_metadata(HashMap::from([("kind".to_string(), "unknown".to_string())]));

        let res = handler.handle(msg).await;
        assert!(matches!(res, Ok(Handled::Ack)));
    }

    #[tokio::test]
    async fn test_typed_handler_failure() {
        let handler = TypeHandler::new().add("fail", |_: TestMsg| async {
            Err(HandlerError::Retryable(anyhow::anyhow!("failure")))
        });

        let msg = CanonicalMessage::from_type(&TestMsg { val: "x".into() })
            .unwrap()
            .with_metadata(HashMap::from([("kind".to_string(), "fail".to_string())]));

        let res = handler.handle(msg).await;
        assert!(matches!(res, Err(HandlerError::Retryable(_))));
    }
}
