//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Builder methods and handlers on a `Route`.

use super::*;

impl Route {
    pub fn with_options(mut self, options: RouteOptions) -> Self {
        self.options = options;
        self
    }
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.options.concurrency = concurrency.max(1);
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.options.batch_size = batch_size.max(1);
        self
    }
    pub fn with_commit_concurrency_limit(mut self, limit: usize) -> Self {
        self.options.commit_concurrency_limit = limit.max(1);
        self
    }

    pub fn with_startup_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.options.startup_timeout_ms = timeout_ms;
        self
    }

    pub fn with_reconnect_interval_ms(mut self, interval_ms: u64) -> Self {
        self.options.reconnect_interval_ms = interval_ms;
        self
    }

    pub fn with_empty_batch_delay_ms(mut self, delay_ms: u64) -> Self {
        self.options.empty_batch_delay_ms = delay_ms;
        self
    }

    /// If true, the route exits gracefully once the source yields an empty
    /// batch (drain-then-exit). Off by default — routes normally poll forever.
    pub fn with_exit_on_empty(mut self, exit_on_empty: bool) -> Self {
        self.options.exit_on_empty = exit_on_empty;
        self
    }

    pub fn with_fault_injection(mut self, allow: bool) -> Self {
        self.options.allow_fault_injection = allow;
        self
    }

    pub fn with_handler(mut self, handler: impl Handler + 'static) -> Self {
        self.output.handler = Some(Arc::new(handler));
        self
    }

    /// Registers a typed handler for the route.
    ///
    /// The handler can accept either:
    /// - `fn(T) -> Future<Output = Result<Handled, HandlerError>>`
    /// - `fn(T, MessageContext) -> Future<Output = Result<Handled, HandlerError>>`
    ///
    /// # Examples
    ///
    /// ```
    /// # use mq_bridge::{Route, models::Endpoint};
    /// # use serde::Deserialize;
    ///
    /// #[derive(Deserialize)]
    /// struct MyData { id: u32 }
    ///
    /// async fn my_handler(data: MyData) -> anyhow::Result<()> {
    ///     Ok(())
    /// }
    ///
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10))
    ///     .add_handler("my_type", my_handler);
    /// ```
    pub fn add_handler<T, H, Args>(mut self, type_name: &str, handler: H) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        H: crate::type_handler::IntoTypedHandler<T, Args>,
        Args: Send + Sync + 'static,
    {
        // Create the wrapper closure that handles deserialization and context extraction
        let handler = Arc::new(handler);
        let wrapper = move |msg: crate::CanonicalMessage| {
            let handler = handler.clone();
            async move {
                let data = msg.parse::<T>().map_err(|e| {
                    HandlerError::NonRetryable(anyhow::anyhow!("Deserialization failed: {}", e))
                })?;
                let ctx = crate::MessageContext::from(msg);
                handler.call(data, ctx).await
            }
        };
        let wrapper = Arc::new(wrapper);

        let prev_handler = self.output.handler.take();

        let new_handler = if let Some(h) = prev_handler {
            if let Some(extended) = h.register_handler(type_name, wrapper.clone()) {
                extended
            } else {
                Arc::new(
                    crate::type_handler::TypeHandler::new()
                        .with_fallback(h)
                        .add_handler(type_name, wrapper),
                )
            }
        } else {
            Arc::new(crate::type_handler::TypeHandler::new().add_handler(type_name, wrapper))
        };

        self.output.handler = Some(new_handler);
        self
    }
    pub fn add_handlers<T, H, Args>(mut self, handlers: HashMap<&str, H>) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        H: crate::type_handler::IntoTypedHandler<T, Args>,
        Args: Send + Sync + 'static,
    {
        for (type_name, handler) in handlers {
            self = self.add_handler(type_name, handler);
        }
        self
    }
}
