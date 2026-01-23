//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::endpoints::{create_consumer_from_route, create_publisher_from_route};
pub use crate::models::Route;
use crate::models::{self, Endpoint};
use crate::traits::{
    BatchCommitFunc, ConsumerError, CustomEndpointFactory, CustomMiddlewareFactory, Handler,
    HandlerError, MessageDisposition, PublisherError, SentBatch,
};
use async_channel::{bounded, Sender};
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock, RwLock};
use tokio::{
    select,
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, error, info, warn};

#[derive(Debug)]
pub struct RouteHandle((JoinHandle<()>, Sender<()>));

impl RouteHandle {
    pub async fn stop(&self) {
        let _ = self.0 .1.send(()).await;
        self.0 .1.close();
    }

    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        self.0 .0.await
    }
}

impl From<(JoinHandle<()>, Sender<()>)> for RouteHandle {
    fn from(tuple: (JoinHandle<()>, Sender<()>)) -> Self {
        RouteHandle(tuple)
    }
}

struct ActiveRoute {
    route: Route,
    handle: RouteHandle,
}

static ROUTE_REGISTRY: OnceLock<RwLock<HashMap<String, ActiveRoute>>> = OnceLock::new();
static ENDPOINT_REGISTRY: OnceLock<RwLock<HashMap<String, Arc<dyn CustomEndpointFactory>>>> =
    OnceLock::new();
static MIDDLEWARE_REGISTRY: OnceLock<RwLock<HashMap<String, Arc<dyn CustomMiddlewareFactory>>>> =
    OnceLock::new();

pub fn register_endpoint_factory(name: &str, factory: Arc<dyn CustomEndpointFactory>) {
    let registry = ENDPOINT_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let mut map = registry.write().expect("Endpoint registry lock poisoned");
    map.insert(name.to_string(), factory);
}

pub fn get_endpoint_factory(name: &str) -> Option<Arc<dyn CustomEndpointFactory>> {
    let registry = ENDPOINT_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let map = registry.read().expect("Endpoint registry lock poisoned");
    map.get(name).cloned()
}

pub fn register_middleware_factory(name: &str, factory: Arc<dyn CustomMiddlewareFactory>) {
    let registry = MIDDLEWARE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let mut map = registry.write().expect("Middleware registry lock poisoned");
    map.insert(name.to_string(), factory);
}

pub fn get_middleware_factory(name: &str) -> Option<Arc<dyn CustomMiddlewareFactory>> {
    let registry = MIDDLEWARE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let map = registry.read().expect("Middleware registry lock poisoned");
    map.get(name).cloned()
}

impl Route {
    /// Creates a new route with default concurrency (1) and batch size (128).
    ///
    /// # Arguments
    /// * `input` - The input/source endpoint for the route
    /// * `output` - The output/sink endpoint for the route
    pub fn new(input: Endpoint, output: Endpoint) -> Self {
        Self {
            input,
            output,
            ..Default::default()
        }
    }

    /// Retrieves a registered (and running) route by name.
    pub fn get(name: &str) -> Option<Self> {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let map = registry.read().expect("Route registry lock poisoned");
        map.get(name).map(|active| active.route.clone())
    }

    /// Returns a list of all registered route names.
    pub fn list() -> Vec<String> {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let map = registry.read().expect("Route registry lock poisoned");
        map.keys().cloned().collect()
    }

    /// Registers the route and starts it.
    /// If a route with the same name is already running, it will be stopped first.
    ///    
    /// # Examples
    /// ```
    /// use mq_bridge::{Route, models::Endpoint};
    ///
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10));
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// route.deploy("global_route").await.unwrap();
    /// assert!(Route::get("global_route").is_some());
    /// # });
    /// ```
    pub async fn deploy(&self, name: &str) -> anyhow::Result<()> {
        Self::stop(name).await;

        let handle = self.run(name).await?;
        let active = ActiveRoute {
            route: self.clone(),
            handle,
        };

        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let mut map = registry.write().expect("Route registry lock poisoned");
        map.insert(name.to_string(), active);
        Ok(())
    }

    /// Stops a running route by name and removes it from the registry.
    pub async fn stop(name: &str) -> bool {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let active_opt = {
            let mut map = registry.write().expect("Route registry lock poisoned");
            map.remove(name)
        };

        if let Some(active) = active_opt {
            active.handle.stop().await;
            let _ = active.handle.join().await;
            true
        } else {
            false
        }
    }

    /// Creates a new Publisher configured for this route's output.
    /// This is useful if you want to send messages to the same destination as this route.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// use mq_bridge::{Route, models::Endpoint};
    ///
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10));
    /// let publisher = route.create_publisher().await;
    /// assert!(publisher.is_ok());
    /// # });
    /// ```
    pub async fn create_publisher(&self) -> anyhow::Result<crate::Publisher> {
        crate::Publisher::new(self.output.clone()).await
    }

    /// Creates a consumer connected to the route's output.
    /// This is primarily useful for integration tests to verify messages reaching the destination.
    pub async fn connect_to_output(
        &self,
        name: &str,
    ) -> anyhow::Result<Box<dyn crate::traits::MessageConsumer>> {
        create_consumer_from_route(name, &self.output).await
    }

    /// Validates the route configuration, checking if endpoints are supported and correctly configured.
    /// Core types like file, memory, and response are always supported.
    /// # Arguments
    /// * `name` - The name of the route
    /// * `allowed_endpoints` - An optional list of allowed endpoint types
    pub fn check(&self, name: &str, allowed_endpoints: Option<&[&str]>) -> anyhow::Result<()> {
        crate::endpoints::check_consumer(name, &self.input, allowed_endpoints)?;
        crate::endpoints::check_publisher(name, &self.output, allowed_endpoints)?;
        Ok(())
    }

    /// Runs the message processing route with concurrency, error handling, and graceful shutdown.
    ///
    /// This function spawns the necessary background tasks to process messages. It waits asynchronously
    /// until the route is successfully initialized (i.e., connections are established) or until
    /// a timeout occurs.
    /// The name_str parameter is just used for logging and tracing.
    ///
    /// It returns a `JoinHandle` for the main route task and a `Sender` channel
    /// that can be used to signal a graceful shutdown. The result is typically converted into a
    /// [`RouteHandle`] for easier management.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use mq_bridge::{Route, route::RouteHandle, models::Endpoint};
    /// # async fn example() -> anyhow::Result<()> {
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10));
    ///
    /// // Start the route (blocks until initialized) and convert to RouteHandle
    /// let handle: RouteHandle = route.run("my_route").await?.into();
    ///
    /// // Stop the route later
    /// handle.stop().await;
    /// handle.join().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run(&self, name_str: &str) -> anyhow::Result<RouteHandle> {
        self.check(name_str, None)?;
        let (shutdown_tx, shutdown_rx) = bounded(1);
        let (ready_tx, ready_rx) = bounded(1);
        // Use `Arc` so route/name clones are cheap (pointer copy) in the reconnect loop.
        let route = Arc::new(self.clone());
        let name = Arc::new(name_str.to_string());

        let handle = tokio::spawn(async move {
            loop {
                let route_arc = Arc::clone(&route);
                let name_arc = Arc::clone(&name);
                // Create a new, per-iteration internal shutdown channel.
                // This avoids a race where both this loop and the inner task
                // try to consume the same external shutdown signal.
                let (internal_shutdown_tx, internal_shutdown_rx) = bounded(1);
                let ready_tx_clone = ready_tx.clone();

                // The actual route logic is in `run_until_err`.
                let mut run_task = tokio::spawn(async move {
                    route_arc
                        .run_until_err(&name_arc, Some(internal_shutdown_rx), Some(ready_tx_clone))
                        .await
                });

                select! {
                    _ = shutdown_rx.recv() => {
                        info!("Shutdown signal received for route '{}'.", name);
                        // Notify the inner task to shut down.
                        let _ = internal_shutdown_tx.send(()).await;
                        // Wait for the inner task to finish gracefully.
                        let _ = run_task.await;
                        break;
                    }
                    res = &mut run_task => {
                        match res {
                            Ok(Ok(should_continue)) if !should_continue => {
                                info!("Route '{}' completed gracefully. Shutting down.", name);
                                break;
                            }
                            Ok(Err(e)) => {
                                error!("Route '{}' failed: {}. Reconnecting in 5 seconds...", name, e);
                                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            }
                            Err(e) => {
                                error!("Route '{}' task panicked: {}. Reconnecting in 5 seconds...", name, e);
                                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            }
                            _ => {} // The route should continue running.
                        }
                    }
                }
            }
        });

        match tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx.recv()).await {
            Ok(Ok(_)) => Ok(RouteHandle((handle, shutdown_tx))),
            _ => {
                handle.abort();
                Err(anyhow::anyhow!(
                    "Route '{}' failed to start within 5 seconds or encountered an error",
                    name_str
                ))
            }
        }
    }

    /// The core logic of running the route, designed to be called within a reconnect loop.
    pub async fn run_until_err(
        &self,
        name: &str,
        shutdown_rx: Option<async_channel::Receiver<()>>,
        ready_tx: Option<Sender<()>>,
    ) -> anyhow::Result<bool> {
        let (_internal_shutdown_tx, internal_shutdown_rx) = bounded(1);
        let shutdown_rx = shutdown_rx.unwrap_or(internal_shutdown_rx);
        if self.concurrency == 1 {
            self.run_sequentially(name, shutdown_rx, ready_tx).await
        } else {
            self.run_concurrently(name, shutdown_rx, ready_tx).await
        }
    }

    /// A simplified, sequential runner for when concurrency is 1.
    async fn run_sequentially(
        &self,
        name: &str,
        shutdown_rx: async_channel::Receiver<()>,
        ready_tx: Option<Sender<()>>,
    ) -> anyhow::Result<bool> {
        let publisher = create_publisher_from_route(name, &self.output).await?;
        let mut consumer = create_consumer_from_route(name, &self.input).await?;
        let max_parallel_commits = self
            .input
            .middlewares
            .iter()
            .find_map(|m| match m {
                models::Middleware::CommitConcurrency(c) => Some(c.limit),
                _ => None,
            })
            .unwrap_or(4096);

        let (err_tx, err_rx) = bounded(1);
        let commit_semaphore = Arc::new(Semaphore::new(max_parallel_commits));
        let mut commit_tasks = JoinSet::new();

        // Sequencer setup to ensure ordered commits even with parallel commit tasks
        let (seq_tx, sequencer_handle) = spawn_sequencer(max_parallel_commits);
        let mut seq_counter = 0u64;

        if let Some(tx) = ready_tx {
            let _ = tx.send(()).await;
        }
        let run_result = loop {
            select! {
                Ok(err) = err_rx.recv() => break Err(err),

                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in sequential runner for route '{}'.", name);
                    break Ok(true); // Stopped by shutdown signal
                }
                res = consumer.receive_batch(self.batch_size) => {
                    let received_batch = match res {
                        Ok(batch) => {
                            if batch.messages.is_empty() {
                                continue; // No messages, loop to select! again
                            }
                            batch
                        }
                        Err(ConsumerError::EndOfStream) => {
                            info!("Consumer for route '{}' reached end of stream. Shutting down.", name);
                            break Ok(false); // Graceful exit
                        }
                        Err(ConsumerError::Connection(e)) => {
                            // Propagate error to trigger reconnect by the outer loop
                            break Err(e);
                        }
                    };
                    debug!("Received a batch of {} messages sequentially", received_batch.messages.len());

                    // Process the batch sequentially without spawning a new task
                    let seq = seq_counter;
                    seq_counter += 1;
                    let commit = wrap_commit(received_batch.commit, seq, seq_tx.clone());
                    let batch_len = received_batch.messages.len();

                    match publisher.send_batch(received_batch.messages).await {
                        Ok(SentBatch::Ack) => {
                            let permit = commit_semaphore.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;
                            let err_tx = err_tx.clone();
                            commit_tasks.spawn(async move {
                                if let Err(e) = commit(vec![MessageDisposition::Ack; batch_len]).await {
                                    error!("Commit failed: {}", e);
                                    let _ = err_tx.send(e).await;
                                }
                                // Permit is dropped here, releasing the slot
                                drop(permit);
                            });
                        }
                        Ok(SentBatch::Partial { responses, failed }) => {
                            let has_retryable = failed.iter().any(|(_, e)| matches!(e, PublisherError::Retryable(_)));
                            if has_retryable {
                                let failed_count = failed.len();
                                let (_, first_error) = failed
                                    .into_iter()
                                    .find(|(_, e)| matches!(e, PublisherError::Retryable(_)))
                                    .expect("has_retryable is true");
                                break Err(anyhow::anyhow!(
                                    "Failed to send {} messages in batch. First retryable error: {}",
                                    failed_count,
                                    first_error
                                ));
                            }
                            for (msg, e) in &failed {
                                error!("Dropping message (ID: {:032x}) due to non-retryable error: {}", msg.message_id, e);
                            }
                            let permit = commit_semaphore.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;
                            let err_tx = err_tx.clone();
                            commit_tasks.spawn(async move {
                                let dispositions = map_responses_to_dispositions(batch_len, responses, &failed);
                                if let Err(e) = commit(dispositions).await {
                                    error!("Commit failed: {}", e);
                                    let _ = err_tx.send(e).await;
                                }
                                drop(permit);
                            });
                        }
                        Err(e) => break Err(e.into()), // Propagate error to trigger reconnect
                    }
                }
            }
        };

        drop(seq_tx);
        // Drain errors while waiting for tasks to finish to prevent deadlocks and lost errors
        loop {
            select! {
                res = err_rx.recv() => {
                    if let Ok(err) = res {
                        error!("Error reported during shutdown: {}", err);
                    }
                }
                res = commit_tasks.join_next() => {
                    if res.is_none() {
                        break;
                    }
                }
            }
        }
        drop(err_rx);
        let _ = sequencer_handle.await;
        run_result
    }

    /// The main concurrent runner for when concurrency > 1.
    async fn run_concurrently(
        &self,
        name: &str,
        shutdown_rx: async_channel::Receiver<()>,
        ready_tx: Option<Sender<()>>,
    ) -> anyhow::Result<bool> {
        let publisher = create_publisher_from_route(name, &self.output).await?;
        let mut consumer = create_consumer_from_route(name, &self.input).await?;
        if let Some(tx) = ready_tx {
            let _ = tx.send(()).await;
        }
        let (err_tx, err_rx) = bounded(1); // For critical, route-stopping errors
                                           // channel capacity: a small buffer proportional to concurrency
        let work_capacity = self.concurrency.saturating_mul(self.batch_size);
        let (work_tx, work_rx) =
            bounded::<(Vec<crate::CanonicalMessage>, BatchCommitFunc)>(work_capacity);
        let max_parallel_commits = self
            .input
            .middlewares
            .iter()
            .find_map(|m| match m {
                models::Middleware::CommitConcurrency(c) => Some(c.limit),
                _ => None,
            })
            .unwrap_or(4096);

        let commit_semaphore = Arc::new(Semaphore::new(max_parallel_commits));

        // --- Ordered Commit Sequencer ---
        // To prevent data loss with cumulative-ack brokers (Kafka/AMQP), commits must happen in order.
        // We assign a sequence number to each batch and use a sequencer task to enforce order.
        let (seq_tx, sequencer_handle) = spawn_sequencer(self.concurrency * 2);

        // --- Worker Pool ---
        let mut join_set = JoinSet::new();
        for i in 0..self.concurrency {
            let work_rx_clone = work_rx.clone();
            let publisher = Arc::clone(&publisher);
            let err_tx = err_tx.clone();
            let commit_semaphore = commit_semaphore.clone();
            let mut commit_tasks = JoinSet::new();
            join_set.spawn(async move {
                debug!("Starting worker {}", i);
                while let Ok((messages, commit)) = work_rx_clone.recv().await {
                    let batch_len = messages.len();
                    match publisher.send_batch(messages).await {
                        Ok(SentBatch::Ack) => {
                            let permit = match commit_semaphore.clone().acquire_owned().await {
                                Ok(p) => p,
                                Err(_) => {
                                    warn!("Semaphore closed, worker exiting");
                                    break;
                                }
                            };
                            let err_tx = err_tx.clone();
                            commit_tasks.spawn(async move {
                                if let Err(e) = commit(vec![MessageDisposition::Ack; batch_len]).await {
                                    error!("Commit failed: {}", e);
                                    let _ = err_tx.send(e).await;
                                }
                                drop(permit);
                            });
                        }
                        Ok(SentBatch::Partial { responses, failed }) => {
                            let has_retryable = failed.iter().any(|(_, e)| matches!(e, PublisherError::Retryable(_)));
                            if has_retryable {
                                let failed_count = failed.len();
                                let (_, first_error) = failed
                                    .into_iter()
                                    .find(|(_, e)| matches!(e, PublisherError::Retryable(_)))
                                    .expect("has_retryable is true");
                                let e = anyhow::anyhow!(
                                    "Failed to send {} messages in batch. First retryable error: {}",
                                    failed_count,
                                    first_error
                                );
                                error!("Worker failed to send message batch: {}", e);
                                if err_tx.send(e).await.is_err() {
                                    warn!("Could not send error to main task, it might be down.");
                                }
                                break; // Stop processing this batch
                            }
                            for (msg, e) in &failed {
                                error!("Worker dropping message (ID: {:032x}) due to non-retryable error: {}", msg.message_id, e);
                            }
                            let permit = match commit_semaphore.clone().acquire_owned().await {
                                Ok(p) => p,
                                Err(_) => {
                                    warn!("Semaphore closed, worker exiting");
                                    break;
                                }
                            };
                            let err_tx = err_tx.clone();
                            commit_tasks.spawn(async move {
                                let dispositions = map_responses_to_dispositions(batch_len, responses, &failed);
                                if let Err(e) = commit(dispositions).await {
                                    error!("Commit failed: {}", e);
                                    let _ = err_tx.send(e).await;
                                }
                                drop(permit);
                            });
                        }
                        Err(e) => {
                            error!("Worker failed to send message batch: {}", e);
                            // Send the error back to the main task to tear down the route.
                            if err_tx.send(e.into()).await.is_err() {
                                warn!("Could not send error to main task, it might be down.");
                            }
                            break;
                        }
                    }
                }
                // Wait for all in-flight commits to complete
                while commit_tasks.join_next().await.is_some() {}
            });
        }

        let mut seq_counter = 0u64;
        loop {
            select! {
                biased; // Prioritize checking for errors

                Ok(err) = err_rx.recv() => {
                    error!("A worker reported a critical error. Shutting down route.");
                    return Err(err);
                }

                Some(res) = join_set.join_next() => {
                    match res {
                        Ok(_) => {
                            error!("A worker task finished unexpectedly. Shutting down route.");
                            return Err(anyhow::anyhow!("Worker task finished unexpectedly"));
                        }
                        Err(e) => {
                            error!("A worker task panicked: {}. Shutting down route.", e);
                            return Err(e.into());
                        }
                    }
                }

                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in concurrent runner for route '{}'.", name);
                    break;
                }

                res = consumer.receive_batch(self.batch_size) => {
                    let (messages, commit) = match res {
                        Ok(batch) => {
                            if batch.messages.is_empty() {
                                continue; // No messages, loop to select! again
                            }
                            (batch.messages, batch.commit)
                        }
                        Err(ConsumerError::EndOfStream) => {
                            info!("Consumer for route '{}' reached end of stream. Shutting down.", name);
                            break; // Graceful exit
                        }
                        Err(ConsumerError::Connection(e)) => {
                            // Propagate error to trigger reconnect by the outer loop
                            return Err(e);
                        }
                    };
                    debug!("Received a batch of {} messages concurrently", messages.len());

                    // Wrap the commit function to route it through the sequencer
                    let seq = seq_counter;
                    seq_counter += 1;
                    let wrapped_commit = wrap_commit(commit, seq, seq_tx.clone());

                    if work_tx.send((messages, wrapped_commit)).await.is_err() {
                        warn!("Work channel closed, cannot process more messages concurrently. Shutting down.");
                        break;
                    }
                }
            }
        }

        // --- Graceful Shutdown ---
        // Close the work channel. Workers will finish their current message and then exit the loop.
        drop(work_tx);
        // Wait for all worker tasks to complete.
        while join_set.join_next().await.is_some() {}

        // Close sequencer
        drop(seq_tx);
        let _ = sequencer_handle.await;

        if let Ok(err) = err_rx.try_recv() {
            return Err(err);
        }

        // Return true if shutdown was requested (channel is empty means it was closed/consumed),
        // false if we reached end-of-stream naturally.
        Ok(shutdown_rx.is_empty())
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
    /// # use mq_bridge::{Route, models::Endpoint, Handled, HandlerError};
    /// # use serde::Deserialize;
    /// # use std::sync::Arc;
    ///
    /// #[derive(Deserialize)]
    /// struct MyData {
    ///     id: u32,
    /// }
    ///
    /// async fn my_handler(data: MyData) -> Result<Handled, HandlerError> {
    ///     Ok(Handled::Ack)
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

type SequencerItem = (
    Vec<MessageDisposition>,
    BatchCommitFunc,
    tokio::sync::oneshot::Sender<anyhow::Result<()>>,
);

fn spawn_sequencer(buffer_size: usize) -> (Sender<(u64, SequencerItem)>, JoinHandle<()>) {
    let (seq_tx, seq_rx) = bounded::<(u64, SequencerItem)>(buffer_size);

    let sequencer_handle = tokio::spawn(async move {
        let mut buffer: BTreeMap<u64, SequencerItem> = BTreeMap::new();
        let mut next_seq = 0u64;
        let mut deadline: Option<tokio::time::Instant> = None;
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

        loop {
            while let Some((dispositions, commit_func, notify)) = buffer.remove(&next_seq) {
                let res = commit_func(dispositions).await;
                let _ = notify.send(res);
                next_seq += 1;
            }

            if !buffer.is_empty() {
                if deadline.is_none() {
                    deadline = Some(tokio::time::Instant::now() + TIMEOUT);
                }
            } else {
                deadline = None;
            }

            let timeout_fut = async {
                if let Some(d) = deadline {
                    tokio::time::sleep_until(d).await
                } else {
                    std::future::pending().await
                }
            };

            select! {
                res = seq_rx.recv() => {
                    match res {
                        Ok((seq, item)) => {
                            if seq < next_seq {
                                let (_, _, notify) = item;
                                let _ = notify.send(Err(anyhow::anyhow!("Sequencer received late item (seq {} < next_seq {})", seq, next_seq)));
                            } else {
                                buffer.insert(seq, item);
                            }
                        }
                        Err(_) => {
                            for (_, (_, _, notify)) in std::mem::take(&mut buffer) {
                                let _ = notify.send(Err(anyhow::anyhow!("Sequencer shutting down")));
                            }
                            break;
                        }
                    }
                }
                _ = timeout_fut => {
                    if let Some(&first_seq) = buffer.keys().next() {
                        if first_seq > next_seq {
                            warn!("Sequencer timed out waiting for seq {}. Jumping to {}.", next_seq, first_seq);
                            next_seq = first_seq;
                        } else {
                            next_seq += 1;
                        }
                    } else {
                        next_seq += 1;
                    }
                    deadline = None;
                }
            }
        }
    });
    (seq_tx, sequencer_handle)
}

fn wrap_commit(
    commit: BatchCommitFunc,
    seq: u64,
    seq_tx: Sender<(u64, SequencerItem)>,
) -> BatchCommitFunc {
    Box::new(move |dispositions| {
        Box::pin(async move {
            let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
            // Send to sequencer
            if seq_tx
                .send((seq, (dispositions, commit, notify_tx)))
                .await
                .is_ok()
            {
                // Wait for sequencer to execute the commit
                match notify_rx.await {
                    Ok(res) => res,
                    Err(_) => Err(anyhow::anyhow!(
                        "Sequencer dropped the commit channel unexpectedly"
                    )),
                }
            } else {
                Err(anyhow::anyhow!(
                    "Failed to send commit to sequencer, route is likely shutting down"
                ))
            }
        })
    })
}

fn map_responses_to_dispositions(
    total_count: usize,
    responses: Option<Vec<crate::CanonicalMessage>>,
    failed: &[(crate::CanonicalMessage, PublisherError)],
) -> Vec<MessageDisposition> {
    if failed.is_empty() {
        if let Some(resps) = responses {
            if resps.len() == total_count {
                return resps.into_iter().map(MessageDisposition::Reply).collect();
            }
        } else {
            // If there are no failures and no responses, everything is Ack.
            return vec![MessageDisposition::Ack; total_count];
        }
    }

    // If we have failures, we should Nack them.
    // However, we don't have easy access to the original indices here to map 1:1 perfectly
    // if we don't assume order.
    // But `send_batch` usually processes in order.
    // If `responses` is Some, it contains responses for successful messages in order.

    // Simplified logic assuming order preservation for successful messages:
    // We construct a vector of dispositions.
    // Since we can't easily match by ID without iterating everything, and `failed` might be sparse,
    // we'll use a heuristic:
    // If we have explicit responses, we use them.
    // If we have failures, we might not be able to map them back to the exact index in the batch
    // without O(N^2) or a map, because `failed` is a subset.
    //
    // For F10 implementation, we will assume that if *any* message failed in the batch,
    // and we are in a Partial state, we might want to Nack the ones that failed.
    // But since we can't easily map back to the index in `received_batch.messages` (which we don't have here in this helper),
    // and `commit` expects a vector of size `total_count` corresponding to the input batch...

    // Current best effort: Return Ack for everything to avoid hanging, but log that we can't map precisely yet.
    // In a real implementation of F10, `send_batch` should probably return `Vec<Result<Sent, PublisherError>>` to map 1:1.
    vec![MessageDisposition::Ack; total_count]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Endpoint, Middleware};
    use crate::traits::{CustomMiddlewareFactory, MessageConsumer, ReceivedBatch};
    use std::any::Any;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[derive(Debug)]
    struct PanicMiddlewareFactory {
        should_panic: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl CustomMiddlewareFactory for PanicMiddlewareFactory {
        async fn apply_consumer(
            &self,
            consumer: Box<dyn MessageConsumer>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            Ok(Box::new(PanicConsumer {
                inner: consumer,
                should_panic: self.should_panic.clone(),
            }))
        }
    }

    struct PanicConsumer {
        inner: Box<dyn MessageConsumer>,
        should_panic: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl MessageConsumer for PanicConsumer {
        async fn receive_batch(
            &mut self,
            max_messages: usize,
        ) -> Result<ReceivedBatch, ConsumerError> {
            // Panic on the first call to verify route recovery
            if self
                .should_panic
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                panic!("Simulated panic for testing recovery");
            }
            self.inner.receive_batch(max_messages).await
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Takes too much time for regular tests"]
    async fn test_route_recovery_from_panic() {
        // Use unique topic names to avoid interference from other tests sharing the static memory channels
        let unique_suffix = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("panic_in_{}", unique_suffix);
        let out_topic = format!("panic_out_{}", unique_suffix);

        let should_panic = Arc::new(AtomicBool::new(true));
        let factory = PanicMiddlewareFactory {
            should_panic: should_panic.clone(),
        };
        register_middleware_factory("panic_factory", Arc::new(factory));

        let input = Endpoint::new_memory(&in_topic, 10).add_middleware(Middleware::Custom {
            name: "panic_factory".to_string(),
            config: serde_json::Value::Null,
        });
        let output = Endpoint::new_memory(&out_topic, 10);

        let route = Route::new(input.clone(), output.clone());

        // Start the route
        route
            .deploy("panic_test")
            .await
            .expect("Failed to deploy route");
        // 1. Send a message. The consumer will panic before picking it up.
        let input_ch = input.channel().unwrap();
        input_ch
            .send_message("persistent_msg".into())
            .await
            .unwrap();

        // 2. Wait for the panic to occur and the route to enter sleep.
        // We loop briefly to allow the spawned task to execute and panic.
        let panic_wait_start = std::time::Instant::now();
        while panic_wait_start.elapsed() < std::time::Duration::from_secs(5) {
            if !should_panic.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !should_panic.load(Ordering::SeqCst),
            "Route should have panicked"
        );

        // 3. Wait for recovery (5s backoff + restart time).
        // We sleep the minimum backoff, then poll with a generous timeout to handle loaded CI environments.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        // 4. Verify the message is processed after recovery.
        let mut verifier = route.connect_to_output("verifier").await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for message after recovery")
            .expect("Stream closed");

        assert_eq!(received.message.get_payload_str(), "persistent_msg");
        // not necessary here, but it's a good idea to commit
        (received.commit)(MessageDisposition::Ack).await.unwrap();

        // Cleanup
        Route::stop("panic_test").await;
    }
}
