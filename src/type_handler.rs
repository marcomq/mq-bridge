use crate::response::ErgonomicResponse;
use crate::traits::{Handled, Handler, HandlerError};
use crate::{CanonicalMessage, MessageContext};
use async_trait::async_trait;
use futures::future::BoxFuture;
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
/// # fn main() {
/// let handler = TypeHandler::new()
///     .add("my_command", |cmd: MyCommand| async move {
///         println!("Received command: {}", cmd.id);
///     }); // No return needed! Implicitly returns (), which maps to Handled::Ack.
/// # }
/// ```
#[derive(Clone)]
pub struct TypeHandler {
    pub(crate) handlers: HashMap<String, Arc<dyn Handler>>,
    pub(crate) type_key: String, // will be the key in msg metadata, default is "kind"
    pub(crate) fallback: Option<Arc<dyn Handler>>,
}

pub const KIND_KEY: &str = "kind";

/// A helper trait to allow registering handlers with or without context.
pub trait IntoTypedHandler<T, Args>: Send + Sync + 'static {
    type Future: Future<Output = Result<Handled, HandlerError>> + Send + 'static;
    fn call(&self, msg: T, ctx: MessageContext) -> Self::Future;
}

/// Implementation for standard closures returning Result<Handled, HandlerError>.
/// This path is unique and allows correct inference for legacy code.
impl<F, Fut, T> IntoTypedHandler<T, (T, Result<Handled, HandlerError>)> for F
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

impl<F, Fut, T> IntoTypedHandler<T, (T, MessageContext, Result<Handled, HandlerError>)> for F
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

/// Ergonomic implementation for closures returning types that implement ErgonomicResponse.
impl<F, Fut, T, R> IntoTypedHandler<T, (T, R)> for F
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: ErgonomicResponse,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
{
    type Future = BoxFuture<'static, Result<Handled, HandlerError>>;
    fn call(&self, msg: T, _ctx: MessageContext) -> Self::Future {
        let fut = (self)(msg);
        Box::pin(async move { fut.await.into_handler_result() })
    }
}

impl<F, Fut, T, R> IntoTypedHandler<T, (T, MessageContext, R)> for F
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: ErgonomicResponse,
    F: Fn(T, MessageContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
{
    type Future = BoxFuture<'static, Result<Handled, HandlerError>>;
    fn call(&self, msg: T, ctx: MessageContext) -> Self::Future {
        let fut = (self)(msg, ctx);
        Box::pin(async move { fut.await.into_handler_result() })
    }
}

impl Default for TypeHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeHandler {
    /// Creates a new TypeHandler that looks for the specified key in message metadata to determine the message type.
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            type_key: KIND_KEY.into(),
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
    pub fn add_simple<T, H, Args>(self, type_name: &str, handler: H) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        H: IntoTypedHandler<T, Args>,
        Args: Send + Sync + 'static,
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
        if let Some(type_val) = msg.get_metadata(&self.type_key) {
            if let Some(handler) = self.handlers.get(type_val) {
                return handler.handle(msg).await;
            }
        }

        if let Some(fallback) = &self.fallback {
            return fallback.handle(msg).await;
        }

        Err(HandlerError::NonRetryable(anyhow::anyhow!(
            "No handler registered for type: '{:?}' and no fallback provided",
            msg.get_metadata(&self.type_key)
        )))
    }

    async fn handle_many(&self, msgs: Vec<CanonicalMessage>) -> Vec<Result<Handled, HandlerError>> {
        #[derive(Clone, PartialEq)]
        enum BatchTarget {
            Handler(String),
            Fallback,
        }

        let mut results: Vec<Option<Result<Handled, HandlerError>>> =
            std::iter::repeat_with(|| None).take(msgs.len()).collect();
        let mut runs: Vec<(BatchTarget, Vec<(usize, CanonicalMessage)>)> = Vec::new();

        for (index, msg) in msgs.into_iter().enumerate() {
            let target = if let Some(type_val) = msg.get_metadata(&self.type_key) {
                if self.handlers.contains_key(type_val) {
                    Some(BatchTarget::Handler(type_val.to_owned()))
                } else if self.fallback.is_some() {
                    Some(BatchTarget::Fallback)
                } else {
                    None
                }
            } else if self.fallback.is_some() {
                Some(BatchTarget::Fallback)
            } else {
                None
            };

            if let Some(target) = target {
                if let Some((last_target, group)) = runs.last_mut() {
                    if *last_target == target {
                        group.push((index, msg));
                        continue;
                    }
                }
                runs.push((target, vec![(index, msg)]));
            } else {
                results[index] = Some(Err(HandlerError::NonRetryable(anyhow::anyhow!(
                    "No handler registered for type: '{:?}' and no fallback provided",
                    msg.get_metadata(&self.type_key)
                ))));
            }
        }

        for (target, group) in runs {
            let expected = group.len();
            let (indices, messages): (Vec<_>, Vec<_>) = group.into_iter().unzip();
            let (handler, label) = match target {
                BatchTarget::Handler(type_name) => {
                    let Some(handler) = self.handlers.get(&type_name) else {
                        for index in indices {
                            results[index] = Some(Err(HandlerError::NonRetryable(
                                anyhow::anyhow!("Handler batch dispatch did not produce a result"),
                            )));
                        }
                        continue;
                    };
                    (handler, format!("Handler for type '{type_name}'"))
                }
                BatchTarget::Fallback => {
                    let Some(handler) = &self.fallback else {
                        for index in indices {
                            results[index] = Some(Err(HandlerError::NonRetryable(
                                anyhow::anyhow!("Handler batch dispatch did not produce a result"),
                            )));
                        }
                        continue;
                    };
                    (handler, "Fallback handler".to_string())
                }
            };

            let group_results = handler.handle_many(messages).await;

            if group_results.len() != expected {
                for index in indices {
                    results[index] = Some(Err(HandlerError::NonRetryable(anyhow::anyhow!(
                        "{} returned {} results for {} messages",
                        label,
                        group_results.len(),
                        expected
                    ))));
                }
                continue;
            }

            for (index, result) in indices.into_iter().zip(group_results) {
                results[index] = Some(result);
            }
        }

        results
            .into_iter()
            .map(|result| {
                result.unwrap_or_else(|| {
                    Err(HandlerError::NonRetryable(anyhow::anyhow!(
                        "Handler batch dispatch did not produce a result"
                    )))
                })
            })
            .collect()
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
    use crate::msg;
    use serde::{Deserialize, Serialize};
    use std::sync::{Arc, Mutex};

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

        let msg = msg!(
            &TestMsg {
                val: "hello".into(),
            },
            "test_a"
        );

        let res = handler.handle(msg).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_typed_handler_with_context() {
        let handler =
            TypeHandler::new().add("test_ctx", |msg: TestMsg, ctx: MessageContext| async move {
                assert_eq!(msg.val, "hello");
                assert_eq!(ctx.get_metadata("meta"), Some("data"));
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
        let msg = msg!(b"{}".to_vec(), "unknown");

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

        let msg = msg!(b"{}".to_vec(), "unknown");

        let res = handler.handle(msg).await;
        assert!(matches!(res, Ok(Handled::Ack)));
    }

    #[tokio::test]
    async fn test_typed_handler_failure() {
        let handler = TypeHandler::new().add("fail", |_: TestMsg| async {
            Result::<Handled, HandlerError>::Err(HandlerError::Retryable(anyhow::anyhow!(
                "failure"
            )))
        });

        let msg = CanonicalMessage::from_type(&TestMsg { val: "x".into() })
            .unwrap()
            .with_type_key("fail");

        let res = handler.handle(msg).await;
        assert!(matches!(res, Err(HandlerError::Retryable(_))));
    }

    #[tokio::test]
    async fn test_typed_handler_missing_type_key() {
        let handler = TypeHandler::new().add("test", |_: TestMsg| async { Ok(Handled::Ack) });

        // Message without "kind" metadata
        let msg = CanonicalMessage::new(b"{}".to_vec(), None);

        let res = handler.handle(msg).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_typed_handler_deserialization_failure() {
        let handler = TypeHandler::new().add("test", |_: TestMsg| async { Ok(Handled::Ack) });

        // Invalid JSON for TestMsg (missing required field)
        let msg = CanonicalMessage::new(b"{}".to_vec(), None)
            .with_metadata(HashMap::from([("kind".to_string(), "test".to_string())]));

        let res = handler.handle(msg).await;
        assert!(matches!(res, Err(HandlerError::NonRetryable(_))));
    }

    #[tokio::test]
    async fn test_typed_handler_handle_many_preserves_dispatch_order() {
        struct RecordingHandler {
            name: &'static str,
            calls: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl Handler for RecordingHandler {
            async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", self.name, msg.get_payload_str()));
                Ok(Handled::Ack)
            }

            async fn handle_many(
                &self,
                msgs: Vec<CanonicalMessage>,
            ) -> Vec<Result<Handled, HandlerError>> {
                let payloads = msgs
                    .iter()
                    .map(|msg| msg.get_payload_str())
                    .collect::<Vec<_>>()
                    .join(",");
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", self.name, payloads));
                msgs.into_iter().map(|_| Ok(Handled::Ack)).collect()
            }
        }

        fn typed_msg(payload: &str, kind: &str) -> CanonicalMessage {
            CanonicalMessage::from(payload)
                .with_metadata(HashMap::from([("kind".to_string(), kind.to_string())]))
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler = TypeHandler::new()
            .add_handler(
                "a",
                RecordingHandler {
                    name: "a",
                    calls: calls.clone(),
                },
            )
            .add_handler(
                "b",
                RecordingHandler {
                    name: "b",
                    calls: calls.clone(),
                },
            );

        let results = handler
            .handle_many(vec![
                typed_msg("a1", "a"),
                typed_msg("a2", "a"),
                typed_msg("b1", "b"),
                typed_msg("a3", "a"),
            ])
            .await;

        assert!(results.into_iter().all(|result| result.is_ok()));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "a:a1,a2".to_string(),
                "b:b1".to_string(),
                "a:a3".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_cqrs_pattern_example() {
        #[derive(Serialize, Deserialize)]
        struct SubmitOrder {
            id: u32,
        }

        #[derive(Serialize, Deserialize)]
        struct OrderSubmitted {
            id: u32,
        }

        let command_bus = TypeHandler::new().add("submit_order", |cmd: SubmitOrder| async move {
            // Execute business logic...
            // Emit event
            let evt = OrderSubmitted { id: cmd.id };
            Ok(Handled::Publish(msg!(&evt, "order_submitted")))
        });

        // Event Handler (Read Side / Projection)
        let projection_handler =
            TypeHandler::new().add("order_submitted", |evt: OrderSubmitted| async move {
                // Update read database / cache
                assert_eq!(evt.id, 101);
                Ok(Handled::Ack)
            });

        // Simulate incoming command
        let cmd = SubmitOrder { id: 101 };
        let cmd_msg = msg!(&cmd, "submit_order");

        // Process command
        let result = command_bus.handle(cmd_msg).await.unwrap();

        if let Handled::Publish(event_msg) = result {
            // Verify event type
            assert_eq!(event_msg.get_metadata("kind"), Some("order_submitted"));

            // Process event (Projection)
            let proj_result = projection_handler.handle(event_msg).await.unwrap();
            assert!(matches!(proj_result, Handled::Ack));
        } else {
            panic!("Expected Handled::Publish");
        }
    }

    #[tokio::test]
    async fn test_cqrs_integration_with_routes() {
        use crate::models::{Endpoint, Route};
        use std::sync::atomic::{AtomicU32, Ordering};

        #[derive(Serialize, Deserialize)]
        struct SubmitOrder {
            id: u32,
        }

        #[derive(Serialize, Deserialize)]
        struct OrderSubmitted {
            id: u32,
        }

        // Shared state to verify projection update
        let read_model_state = Arc::new(AtomicU32::new(0));
        let read_model_clone = read_model_state.clone();

        let command_handler =
            TypeHandler::new().add("submit_order", |cmd: SubmitOrder| async move {
                let evt = OrderSubmitted { id: cmd.id };
                Ok(Handled::Publish(msg!(&evt, "order_submitted")))
            });

        let event_handler =
            TypeHandler::new().add("order_submitted", move |evt: OrderSubmitted| {
                let state = read_model_clone.clone();
                async move {
                    state.store(evt.id, Ordering::SeqCst);
                    Ok(Handled::Ack)
                }
            });

        let cmd_in_ep = Endpoint::new_memory("cmd_in", 10);
        let event_bus_ep = Endpoint::new_memory("event_bus", 10);
        let proj_out_ep = Endpoint::new_memory("proj_out", 10);

        let command_route =
            Route::new(cmd_in_ep.clone(), event_bus_ep.clone()).with_handler(command_handler);

        let event_route =
            Route::new(event_bus_ep.clone(), proj_out_ep.clone()).with_handler(event_handler);

        let h1 = tokio::spawn(async move {
            command_route
                .run_until_err("command_route", None, None)
                .await
        });
        let h2 =
            tokio::spawn(async move { event_route.run_until_err("event_route", None, None).await });

        let cmd_channel = cmd_in_ep.channel().unwrap();
        let cmd = SubmitOrder { id: 777 };
        let msg = CanonicalMessage::from_type(&cmd)
            .unwrap()
            .with_type_key("submit_order");
        cmd_channel.send_message(msg).await.unwrap();

        let mut attempts = 0;
        while read_model_state.load(Ordering::SeqCst) != 777 && attempts < 50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            attempts += 1;
        }

        assert_eq!(read_model_state.load(Ordering::SeqCst), 777);

        // Cleanup
        cmd_channel.close();
        event_bus_ep.channel().unwrap().close();

        let _ = h1.await;
        let _ = h2.await;
    }
}
