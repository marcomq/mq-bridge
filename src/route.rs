//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::endpoints::{create_consumer_from_route, create_publisher_from_route};
use crate::errors::ProcessingError;
pub use crate::models::Route;
use crate::models::{Endpoint, EndpointType, RouteOptions};
use crate::traits::{
    BatchCommitFunc, ConsumerError, Handler, HandlerError, MessageDisposition, PublisherError,
    SentBatch,
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

// Re-export extensions for backward compatibility and internal usage
pub use crate::extensions::{
    get_endpoint_factory, get_middleware_factory, register_endpoint_factory,
    register_middleware_factory,
};

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
static ENDPOINT_REF_REGISTRY: OnceLock<RwLock<HashMap<String, Endpoint>>> = OnceLock::new();

/// Registers a named endpoint that can be referenced by other endpoints using `ref: "name"`.
/// This will overwrite any existing endpoint with the same name.
pub fn register_endpoint(name: &str, endpoint: Endpoint) {
    let registry = ENDPOINT_REF_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let mut writer = registry
        .write()
        .expect("Named endpoint registry lock poisoned");
    if writer.insert(name.to_string(), endpoint).is_some() {
        debug!("Overwriting a registered endpoint named '{}'", name);
    }
}

/// Retrieves a registered endpoint by name.
pub fn get_endpoint(name: &str) -> Option<Endpoint> {
    let registry = ENDPOINT_REF_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let reader = registry
        .read()
        .expect("Named endpoint registry lock poisoned");
    reader.get(name).cloned()
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

    /// Returns true if the input is of type ref (and the output isn't)
    pub fn is_ref(&self) -> bool {
        matches!(self.input.endpoint_type, EndpointType::Ref(_))
            && !matches!(self.output.endpoint_type, EndpointType::Ref(_))
    }

    /// Registers the route's output endpoint under the given name.
    /// This allows other routes to reference this output using `ref: "name"`.
    pub fn register_output_endpoint(&self, name: Option<&str>) -> Result<(), anyhow::Error> {
        match name {
            Some(name) => {
                register_endpoint(name, self.output.clone());
            }
            None => {
                if let EndpointType::Ref(name) = &self.input.endpoint_type {
                    register_endpoint(name, self.output.clone());
                } else {
                    return Err(anyhow::anyhow!(
                        "No name and input is not a reference endpoint"
                    ));
                }
            }
        };
        Ok(())
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
    pub fn check(
        &self,
        name: &str,
        allowed_endpoints: Option<&[&str]>,
    ) -> anyhow::Result<Vec<String>> {
        let mut warnings = Vec::new();
        warnings.extend(crate::endpoints::check_consumer(
            name,
            &self.input,
            allowed_endpoints,
        )?);
        warnings.extend(crate::endpoints::check_publisher(
            name,
            &self.output,
            allowed_endpoints,
        )?);
        Ok(warnings)
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
        let warnings = self.check(name_str, None)?;
        for warning in warnings {
            tracing::warn!(route = name_str, "Configuration warning: {}", warning);
        }
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
                                match e.downcast_ref::<ProcessingError>() {
                                    Some(ProcessingError::Retryable(_)) => {
                                        warn!("Route '{}' failed with a retryable error: {}. Reconnecting in 5 seconds...", name, e);
                                        break;
                                    }
                                    _ => {
                                        error!("Route '{}' failed: {}. Reconnecting in 5 seconds...", name, e);
                                    }
                                }
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
        if self.options.concurrency == 1 {
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
        let (err_tx, err_rx) = bounded(1);
        let commit_semaphore = Arc::new(Semaphore::new(self.options.commit_concurrency_limit));
        let mut commit_tasks = JoinSet::new();

        // Sequencer setup to ensure ordered commits even with parallel commit tasks
        let (seq_tx, sequencer_handle) = spawn_sequencer(self.options.commit_concurrency_limit);
        let mut seq_counter = 0u64;

        if let Some(tx) = ready_tx {
            let _ = tx.send(()).await;
        }
        let mut message_ids = Vec::with_capacity(self.options.batch_size);
        let run_result = loop {
            select! {
                Ok(err) = err_rx.recv() => break Err(err),

                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in sequential runner for route '{}'.", name);
                    break Ok(true); // Stopped by shutdown signal
                }
                res = consumer.receive_batch(self.options.batch_size) => {
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
                        },
                        Err(ConsumerError::Gap { requested, base }) => {
                            // Propagate gap error to trigger reconnect by the outer loop
                            break Err(anyhow::anyhow!("Consumer gap: requested offset {requested} but earliest available is {base}"));
                        }
                    };
                    debug!("Received a batch of {} messages sequentially", received_batch.messages.len());

                    // Process the batch sequentially without spawning a new task
                    let seq = seq_counter;
                    seq_counter += 1;
                    let commit = wrap_commit(received_batch.commit, seq, seq_tx.clone());
                    let batch_len = received_batch.messages.len();
                    message_ids.clear();
                    message_ids.extend(received_batch.messages.iter().map(|m| m.message_id));

                    match publisher.send_batch(received_batch.messages).await {
                        Ok(SentBatch::Ack) => {
                            let permit = commit_semaphore.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;
                            let err_tx = err_tx.clone();
                            commit_tasks.spawn(async move {
                                if let Err(e) = commit(vec![MessageDisposition::Ack; batch_len]).await {
                                    error!("Commit failed: {}", e);
                                    if err_tx.try_send(e).is_err() {
                                        warn!("Could not send commit error to main task, it might be down or busy.");
                                    }
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
                                    .iter()
                                    .find(|(_, e)| matches!(e, PublisherError::Retryable(_)))
                                    .expect("has_retryable is true");
                                let err = anyhow::anyhow!(
                                    "Failed to send {} messages in batch. First retryable error: {}",
                                    failed_count,
                                    first_error
                                );
                                // Partially ack/nack the batch to fill the sequencer slot.
                                let dispositions =
                                    map_responses_to_dispositions(&message_ids, responses, &failed);
                                if let Err(commit_err) = commit(dispositions).await {
                                    warn!("Commit after partial send failure also failed (this is expected during a disconnect): {}", commit_err);
                                }

                                break Err(err);
                            }
                            for (msg, e) in &failed {
                                error!("Dropping message (ID: {:032x}) due to non-retryable error: {}", msg.message_id, e);
                            }
                            let permit = commit_semaphore.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;
                            let err_tx = err_tx.clone();
                            let ids = std::mem::take(&mut message_ids);
                            commit_tasks.spawn(async move {
                                let dispositions = map_responses_to_dispositions(&ids, responses, &failed);
                                if let Err(e) = commit(dispositions).await {
                                    error!("Commit failed: {}", e);
                                    if err_tx.try_send(e).is_err() {
                                        warn!("Could not send commit error to main task, it might be down or busy.");
                                    }
                                }
                                drop(permit);
                            });
                        }
                        Err(e) => {
                            warn!("Publisher error, sending {} Nacks to commit", batch_len);
                            let nack_result = commit(vec![MessageDisposition::Nack; batch_len]).await;
                            debug!("Nack commit result: {:?}", nack_result);
                            break Err(e.into());
                        }
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
        let work_capacity = self
            .options
            .concurrency
            .saturating_mul(self.options.batch_size);
        let (work_tx, work_rx) =
            bounded::<(Vec<crate::CanonicalMessage>, BatchCommitFunc)>(work_capacity);
        let commit_semaphore = Arc::new(Semaphore::new(self.options.commit_concurrency_limit));

        // --- Ordered Commit Sequencer ---
        // To prevent data loss with cumulative-ack brokers (Kafka/AMQP), commits must happen in order.
        // We assign a sequence number to each batch and use a sequencer task to enforce order.
        let (seq_tx, sequencer_handle) = spawn_sequencer(self.options.commit_concurrency_limit);

        // --- Worker Pool ---
        let batch_size = self.options.batch_size;
        let mut join_set = JoinSet::new();
        for i in 0..self.options.concurrency {
            let work_rx_clone = work_rx.clone();
            let publisher = Arc::clone(&publisher);
            let err_tx = err_tx.clone();
            let commit_semaphore = commit_semaphore.clone();
            let mut commit_tasks = JoinSet::new();
            join_set.spawn(async move {
                debug!("Starting worker {}", i);
                let mut message_ids = Vec::with_capacity(batch_size);
                while let Ok((messages, commit)) = work_rx_clone.recv().await {
                    let batch_len = messages.len();
                    message_ids.clear();
                    message_ids.extend(messages.iter().map(|m| m.message_id));
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
                                    if err_tx.try_send(e).is_err() {
                                        warn!("Could not send commit error to main task, it might be down or busy.");
                                    }
                                }
                                drop(permit);
                            });
                        }
                        Ok(SentBatch::Partial { responses, failed }) => {
                            let has_retryable = failed.iter().any(|(_, e)| matches!(e, PublisherError::Retryable(_)));
                            if has_retryable {
                                let failed_count = failed.len();
                                let (_, first_error) = failed
                                    .iter()
                                    .find(|(_, e)| matches!(e, PublisherError::Retryable(_)))
                                    .expect("has_retryable is true");
                                let e = anyhow::anyhow!(
                                    "Failed to send {} messages in batch. First retryable error: {}",
                                    failed_count,
                                    first_error
                                );
                                error!("Worker failed to send message batch: {}", e);
                                // Partially ack/nack the batch to fill the sequencer slot.
                                // This is crucial to prevent deadlocks. Even if this commit fails
                                // due to a broken connection, it's important to attempt it.
                                let dispositions =
                                    map_responses_to_dispositions(&message_ids, responses, &failed);
                                if let Err(commit_err) = commit(dispositions).await {
                                    warn!("Commit after partial send failure also failed (this is expected during a disconnect): {}", commit_err);
                                }

                                if err_tx.try_send(e).is_err() {
                                    warn!("Could not send error to main task, it might be down or busy.");
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
                            let ids = std::mem::take(&mut message_ids);
                            commit_tasks.spawn(async move {
                                let dispositions = map_responses_to_dispositions(&ids, responses, &failed);
                                if let Err(e) = commit(dispositions).await {
                                    error!("Commit failed: {}", e);
                                    if err_tx.try_send(e).is_err() {
                                        warn!("Could not send commit error to main task, it might be down or busy.");
                                    }
                                }
                                drop(permit);
                            });
                        }
                        Err(e) => {
                            error!("Worker failed to send message batch: {}", e);
                            // Nack the commit to fill the sequencer slot and prevent a deadlock.
                            let nack_result = commit(vec![MessageDisposition::Nack; batch_len]).await;
                            debug!("Nack commit result: {:?}", nack_result);
                            // Send the error back to the main task to tear down the route.
                            if err_tx.try_send(e.into()).is_err() {
                                warn!("Could not send error to main task, it might be down or busy.");
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
        // Holds an error that caused the loop to break, to be returned after graceful shutdown.
        let mut loop_error: Option<anyhow::Error> = None;
        loop {
            select! {
                biased; // Prioritize checking for errors

                Ok(err) = err_rx.recv() => {
                    error!("A worker reported a critical error. Shutting down route.");
                    loop_error = Some(err);
                    break;
                }

                Some(res) = join_set.join_next() => {
                    match res {
                        Ok(_) => {
                            error!("A worker task finished unexpectedly. Shutting down route.");
                            loop_error = Some(anyhow::anyhow!("Worker task finished unexpectedly"));
                        }
                        Err(e) => {
                            error!("A worker task panicked: {}. Shutting down route.", e);
                            loop_error = Some(e.into());
                        }
                    }
                    break;
                }

                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in concurrent runner for route '{}'.", name);
                    break;
                }

                res = consumer.receive_batch(self.options.batch_size) => {
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
                            loop_error = Some(e);
                            break;
                        }
                        Err(ConsumerError::Gap { requested, base }) => {
                            // Propagate gap error to trigger reconnect by the outer loop
                            loop_error = Some(ConsumerError::Gap { requested, base }.into());
                            break;
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
        // Close the work channel so workers drain their current messages and exit the loop.
        // This applies on both normal shutdown AND error paths, ensuring in-flight commits
        // are not aborted mid-sequence.
        drop(work_tx);
        // Wait for all worker tasks to complete.
        while join_set.join_next().await.is_some() {}

        // Close sequencer
        drop(seq_tx);
        let _ = sequencer_handle.await;

        if let Some(err) = loop_error {
            return Err(err);
        }

        if let Ok(err) = err_rx.try_recv() {
            return Err(err);
        }

        // Return true if shutdown was requested (channel is empty means it was closed/consumed),
        // false if we reached end-of-stream naturally.
        Ok(shutdown_rx.is_empty())
    }

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
                                // This should not happen with the new timeout logic, but we'll keep the check.
                                let _ = notify.send(Err(anyhow::anyhow!("Sequencer received late item (seq {} < next_seq {}), which is unexpected", seq, next_seq)));
                            } else {
                                buffer.insert(seq, item);
                            }
                        }
                        Err(_) => {
                            for (_, (_, _, notify)) in std::mem::take(&mut buffer) {
                                let _ = notify.send(Err(anyhow::anyhow!("Sequencer is shutting down")));
                            }
                            break;
                        }
                    }
                }
                _ = timeout_fut => {
                    if let Some(first_seq) = buffer.keys().next() {
                        // With the Nack-on-failure fix, all sequence slots are always filled before
                        // a worker exits. A timeout here indicates a bug: a commit was lost without
                        // being called. The sequencer will keep waiting to avoid skipping messages.
                        error!("Sequencer timed out waiting for seq {}. Next in buffer is {}. This is a bug - a commit slot was never filled.", next_seq, *first_seq);
                    } else {
                        // This should be unreachable as a timeout can't occur on an empty buffer.
                        error!("Sequencer timed out on an empty buffer, which is unexpected.");
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
    let commit: Arc<BatchCommitFunc> = Arc::from(commit);
    Box::new(move |dispositions| {
        let commit = commit.clone();
        let seq_tx = seq_tx.clone();
        Box::pin(async move {
            let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
            // Send to sequencer
            let commit_wrapper: BatchCommitFunc = Box::new(move |d| {
                let commit = commit.clone();
                commit(d)
            });
            if seq_tx
                .send((seq, (dispositions, commit_wrapper, notify_tx)))
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
    message_ids: &[u128],
    responses: Option<Vec<crate::CanonicalMessage>>,
    failed: &[(crate::CanonicalMessage, PublisherError)],
) -> Vec<MessageDisposition> {
    let mut dispositions = Vec::with_capacity(message_ids.len());
    let failed_ids: std::collections::HashSet<u128> =
        failed.iter().map(|(m, _)| m.message_id).collect();

    // Create a map from message_id to response message for efficient lookup.
    let mut response_map: std::collections::HashMap<u128, crate::CanonicalMessage> = responses
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.message_id, r))
        .collect();

    for id in message_ids {
        if failed_ids.contains(id) {
            dispositions.push(MessageDisposition::Nack);
        } else if let Some(resp) = response_map.remove(id) {
            // If a response exists for this specific ID, use it.
            dispositions.push(MessageDisposition::Reply(resp));
        } else {
            // Otherwise, it was a successful send that did not produce a response.
            dispositions.push(MessageDisposition::Ack);
        }
    }
    dispositions
}

#[cfg(test)]
fn test_map_responses_to_dispositions_logic() {
    use crate::{traits::PublisherError, CanonicalMessage};
    use anyhow::anyhow;

    let ids = vec![1, 2, 3, 4];

    let mut resp1 = CanonicalMessage::from("resp1");
    resp1.message_id = 1;
    let mut resp4 = CanonicalMessage::from("resp4");
    resp4.message_id = 4;

    let responses = Some(vec![
        resp1, // Corresponds to id 1
        resp4, // Corresponds to id 4
    ]);

    let mut msg2 = CanonicalMessage::from("msg2");
    msg2.message_id = 2;
    let failed = vec![(msg2, PublisherError::NonRetryable(anyhow!("failed")))];

    let dispositions = map_responses_to_dispositions(&ids, responses, &failed);

    assert_eq!(dispositions.len(), 4);
    assert!(matches!(dispositions[0], MessageDisposition::Reply(_))); // from responses
    assert!(matches!(dispositions[1], MessageDisposition::Nack)); // from failed
    assert!(matches!(dispositions[2], MessageDisposition::Ack)); // implicit ack
    assert!(matches!(dispositions[3], MessageDisposition::Reply(_))); // from responses
}

pub fn get_route(name: &str) -> Option<Route> {
    Route::get(name)
}

pub fn list_routes() -> Vec<String> {
    Route::list()
}

pub async fn stop_route(name: &str) -> bool {
    Route::stop(name).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Endpoint, EndpointType, FaultMode, Middleware, RandomPanicMiddleware};
    use crate::traits::{
        CustomMiddlewareFactory, MessageConsumer, MessagePublisher, ReceivedBatch,
    };
    use crate::CanonicalMessage;
    use std::any::Any;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Helper function to run a fault injection test on the consumer side.
    async fn run_consumer_fault_test(
        mode: FaultMode,
        expected_payload: &str,
        route_should_restart: bool,
        concurrency: usize,
    ) {
        let unique_suffix = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("fault_in_{}_{}_{}", mode, concurrency, unique_suffix);
        let out_topic = format!("fault_out_{}_{}_{}", mode, concurrency, unique_suffix);

        let fault_config = RandomPanicMiddleware {
            mode,
            trigger_on_message: Some(1), // Panic on the first message
            enabled: true,
            ..Default::default()
        };

        let input = Endpoint::new_memory(&in_topic, 10)
            .add_middleware(Middleware::RandomPanic(fault_config));
        let output = Endpoint::new_memory(&out_topic, 10);

        let route_name = format!("fault_test_{}_{}", mode, concurrency);
        let route = Route::new(input.clone(), output.clone()).with_concurrency(concurrency);

        // Start the route
        route
            .deploy(&route_name)
            .await
            .expect("Failed to deploy route");
        // Send a message. The consumer will inject a fault when it tries to receive it.
        let input_ch = input.channel().unwrap();
        input_ch
            .send_message("persistent_msg".into())
            .await
            .unwrap();

        if route_should_restart {
            // The route's worker will fail, and the supervisor will wait 5 seconds before restarting.
            // We wait for a bit longer than that to ensure recovery has happened.
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        } else {
            // Route doesn't restart, just wait a bit for the (faulty) message to pass through.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        // Verify the outcome.
        let mut verifier = route.connect_to_output("verifier").await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for message after fault")
            .expect("Stream closed while waiting for message");

        assert_eq!(received.message.get_payload_str(), expected_payload);
        (received.commit)(MessageDisposition::Ack).await.unwrap();

        // Cleanup
        Route::stop(&route_name).await;
    }

    // Helper function to run a fault injection test on the publisher side.
    async fn run_publisher_fault_test(
        mode: FaultMode,
        expected_payload: &str,
        route_should_restart: bool,
    ) {
        let unique_suffix = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("pub_fault_in_{}_{}", mode, unique_suffix);
        let out_topic = format!("pub_fault_out_{}_{}", mode, unique_suffix);

        let fault_config = RandomPanicMiddleware {
            mode,
            trigger_on_message: Some(1), // Trigger on the first message
            enabled: true,
            ..Default::default()
        };

        let mut input = Endpoint::new_memory(&in_topic, 10);
        // Enable NACK on input so messages aren't lost when publisher crashes
        if let EndpointType::Memory(ref mut cfg) = input.endpoint_type {
            cfg.enable_nack = true;
        }
        // Apply fault middleware to output
        let output = Endpoint::new_memory(&out_topic, 10)
            .add_middleware(Middleware::RandomPanic(fault_config));

        let route_name = format!("pub_fault_test_{}", mode);
        let route = Route::new(input.clone(), output.clone());

        route
            .deploy(&route_name)
            .await
            .expect("Failed to deploy route");

        let input_ch = input.channel().unwrap();
        input_ch
            .send_message(expected_payload.into())
            .await
            .unwrap();

        if route_should_restart {
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        let mut verifier = route.connect_to_output("verifier").await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for message after publisher fault")
            .expect("Stream closed");

        assert_eq!(received.message.get_payload_str(), expected_payload);
        (received.commit)(MessageDisposition::Ack).await.unwrap();

        Route::stop(&route_name).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Takes too much time for regular tests"]
    async fn test_route_recovery_from_faults() {
        let original_payload = "persistent_msg";

        // Test with concurrency > 1
        run_consumer_fault_test(FaultMode::Panic, original_payload, true, 2).await;
        run_consumer_fault_test(FaultMode::Disconnect, original_payload, true, 2).await;
        run_consumer_fault_test(FaultMode::Timeout, original_payload, true, 2).await;
        run_consumer_fault_test(FaultMode::Nack, original_payload, true, 2).await;

        // This fault replaces the message but does not restart the route.
        run_consumer_fault_test(FaultMode::JsonFormatError, "{invalid json}", false, 2).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Takes too much time for regular tests"]
    async fn test_route_recovery_from_faults_sequential() {
        let original_payload = "persistent_msg";

        // Test with concurrency = 1
        run_consumer_fault_test(FaultMode::Panic, original_payload, true, 1).await;
        run_consumer_fault_test(FaultMode::Disconnect, original_payload, true, 1).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Takes too much time for regular tests"]
    async fn test_publisher_recovery_from_faults() {
        let original_payload = "persistent_msg";
        // Test publisher-side faults causing restart/retry.
        // `FaultMode::Panic` is not tested here because the `MemoryConsumer` used for input
        // does not support crash-safe at-least-once delivery. A panic in the publisher
        // worker would cause the in-flight message to be lost.
        run_publisher_fault_test(FaultMode::Disconnect, original_payload, true).await;
        run_publisher_fault_test(FaultMode::Timeout, original_payload, true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_route_sequencer_deadlock_fix() {
        // This test ensures that when a worker fails to send a batch (and thus drops the commit handle),
        // the sequencer doesn't deadlock waiting for that sequence number.
        // The fix ensures that even on failure, the commit function is called (with Nack) to fill the sequence gap.

        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("fail_factory_{}", unique_id);
        let in_topic = format!("deadlock_in_{}", unique_id);
        let out_topic = format!("deadlock_out_{}", unique_id);

        #[derive(Debug)]
        struct FailingMiddlewareFactory {
            fail_flag: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl CustomMiddlewareFactory for FailingMiddlewareFactory {
            async fn apply_publisher(
                &self,
                publisher: Box<dyn MessagePublisher>,
                _route_name: &str,
                _config: &serde_json::Value,
            ) -> anyhow::Result<Box<dyn MessagePublisher>> {
                Ok(Box::new(FailingPublisher {
                    inner: publisher,
                    fail_flag: self.fail_flag.clone(),
                }))
            }
            async fn apply_consumer(
                &self,
                consumer: Box<dyn MessageConsumer>,
                _route_name: &str,
                _config: &serde_json::Value,
            ) -> anyhow::Result<Box<dyn MessageConsumer>> {
                Ok(consumer)
            }
        }

        struct FailingPublisher {
            inner: Box<dyn MessagePublisher>,
            fail_flag: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl MessagePublisher for FailingPublisher {
            async fn send_batch(
                &self,
                messages: Vec<crate::CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                // We want to fail one batch to trigger the error path in the worker.
                // We use compare_exchange to ensure only one failure happens.
                if self
                    .fail_flag
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Err(PublisherError::Retryable(anyhow::anyhow!(
                        "Simulated failure"
                    )));
                }
                // Add a small delay for successful batches to ensure the failed one (if it created a gap)
                // would block the sequencer if the gap wasn't filled.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                self.inner.send_batch(messages).await
            }
            async fn send(
                &self,
                msg: crate::CanonicalMessage,
            ) -> Result<crate::traits::Sent, PublisherError> {
                self.inner.send(msg).await
            }
            async fn flush(&self) -> anyhow::Result<()> {
                self.inner.flush().await
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let fail_flag = Arc::new(AtomicBool::new(true));
        register_middleware_factory(
            &factory_name,
            Arc::new(FailingMiddlewareFactory {
                fail_flag: fail_flag.clone(),
            }),
        );

        let input = Endpoint::new_memory(&in_topic, 100);
        let output = Endpoint::new_memory(&out_topic, 100).add_middleware(Middleware::Custom {
            name: factory_name,
            config: serde_json::Value::Null,
        });

        // Concurrency > 1 is required to have multiple workers and potential out-of-order completion
        let route = Route::new(input.clone(), output.clone())
            .with_concurrency(2)
            .with_batch_size(1);

        // Send messages
        let input_ch = input.channel().unwrap();
        input_ch.send_message("msg1".into()).await.unwrap();
        input_ch.send_message("msg2".into()).await.unwrap();
        input_ch.send_message("msg3".into()).await.unwrap();

        // Run the route. It should fail eventually due to the simulated error,
        // but it MUST NOT deadlock.
        let run_fut = async {
            let (_shutdown_tx, shutdown_rx) = async_channel::bounded(1);
            route
                .run_until_err("deadlock_test", Some(shutdown_rx), None)
                .await
        };

        // If deadlock exists, this timeout will trigger.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_fut).await;

        match result {
            Ok(res) => {
                // We expect an error because the publisher returns Err.
                assert!(
                    res.is_err(),
                    "Route should have failed with simulated error"
                );
            }
            Err(_) => {
                panic!("Route deadlocked! The sequencer likely didn't receive the Nack for the failed batch.");
            }
        }
    }

    use crate::traits::{CustomEndpointFactory, Sent};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    type ConsumerBehavior =
        Arc<Mutex<dyn FnMut() -> Result<Box<dyn MessageConsumer>, anyhow::Error> + Send + Sync>>;
    type PublisherBehavior =
        Arc<Mutex<dyn FnMut() -> Result<Box<dyn MessagePublisher>, anyhow::Error> + Send + Sync>>;

    struct MockEndpointFactory {
        create_consumer_fail: bool,
        consumer_behavior: ConsumerBehavior,
        publisher_behavior: PublisherBehavior,
    }

    impl std::fmt::Debug for MockEndpointFactory {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockEndpointFactory")
                .field("create_consumer_fail", &self.create_consumer_fail)
                .finish()
        }
    }

    impl MockEndpointFactory {
        fn new() -> Self {
            Self {
                create_consumer_fail: false,
                consumer_behavior: Arc::new(Mutex::new(|| Err(anyhow::anyhow!("Not implemented")))),
                publisher_behavior: Arc::new(Mutex::new(|| {
                    Ok(Box::new(NoOpPublisher) as Box<dyn MessagePublisher>)
                })),
            }
        }
    }

    #[derive(Clone)]
    struct NoOpPublisher;
    #[async_trait::async_trait]
    impl MessagePublisher for NoOpPublisher {
        async fn send_batch(
            &self,
            _: Vec<crate::CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            Ok(SentBatch::Ack)
        }
        async fn send(&self, _: crate::CanonicalMessage) -> Result<Sent, PublisherError> {
            Ok(Sent::Ack)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[async_trait::async_trait]
    impl CustomEndpointFactory for MockEndpointFactory {
        async fn create_consumer(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            if self.create_consumer_fail {
                return Err(anyhow::anyhow!("Endpoint unavailable"));
            }
            (self.consumer_behavior.lock().unwrap())()
        }
        async fn create_publisher(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            (self.publisher_behavior.lock().unwrap())()
        }
    }

    #[tokio::test]
    async fn test_start_fails_on_unavailable_endpoint() {
        // tokio::time::pause();
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("unavailable_{}", unique_id);

        let factory = Arc::new(MockEndpointFactory {
            create_consumer_fail: true,
            ..MockEndpointFactory::new()
        });
        register_endpoint_factory(&factory_name, factory);

        let input = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let output = Endpoint::new_memory("out", 10);
        let route = Route::new(input, output);

        // The route should fail to start because the input endpoint fails to create.
        // The run() method waits for a ready signal which never comes.
        let result = route.run("test_start_fail").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to start"));
    }

    #[tokio::test]
    async fn test_reconnect_on_consumer_error() {
        // tokio::time::pause();
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let factory_name = format!("reconnect_{}", unique_id);

        // Shared state to track connection attempts
        let connection_attempts = Arc::new(AtomicUsize::new(0));
        let attempts_clone = connection_attempts.clone();

        let consumer_logic = move || -> Result<Box<dyn MessageConsumer>, anyhow::Error> {
            let attempt = attempts_clone.fetch_add(1, Ordering::SeqCst);

            struct FlakyConsumer {
                attempt: usize,
            }
            #[async_trait::async_trait]
            impl MessageConsumer for FlakyConsumer {
                async fn receive_batch(
                    &mut self,
                    _max: usize,
                ) -> Result<ReceivedBatch, ConsumerError> {
                    if self.attempt == 0 {
                        // First connection works for one batch, then fails
                        self.attempt = 999; // prevent infinite loop in this instance
                        Ok(ReceivedBatch {
                            messages: vec![crate::CanonicalMessage::from("msg1")],
                            commit: Box::new(|_| Box::pin(async { Ok(()) })),
                        })
                    } else if self.attempt == 999 {
                        // Simulate connection drop
                        Err(ConsumerError::Connection(anyhow::anyhow!(
                            "Connection dropped"
                        )))
                    } else {
                        // Subsequent connections work
                        // Sleep a bit to prevent busy loop in test
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        Ok(ReceivedBatch {
                            messages: vec![crate::CanonicalMessage::from("msg2")],
                            commit: Box::new(|_| Box::pin(async { Ok(()) })),
                        })
                    }
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(FlakyConsumer { attempt }))
        };

        let mut factory = MockEndpointFactory::new();
        factory.consumer_behavior = Arc::new(Mutex::new(consumer_logic));
        register_endpoint_factory(&factory_name, Arc::new(factory));

        let input = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: factory_name,
                config: serde_json::Value::Null,
            },
            middlewares: vec![],
            handler: None,
        };
        let output = Endpoint::new_memory(&format!("out_{}", unique_id), 10);
        let route = Route::new(input, output.clone());

        route.deploy("test_reconnect").await.unwrap();

        // Wait for reconnection and messages
        let mut verifier = create_consumer_from_route("verifier", &output)
            .await
            .unwrap();

        // Should receive msg1
        let msg1 = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for msg1")
            .unwrap();
        assert_eq!(msg1.message.get_payload_str(), "msg1");

        // Route encounters error, sleeps 5s (skipped by pause), reconnects.
        // Should receive msg2
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out waiting for msg2")
            .unwrap();
        assert_eq!(msg2.message.get_payload_str(), "msg2");

        assert!(connection_attempts.load(Ordering::SeqCst) >= 2);
        Route::stop("test_reconnect").await;
    }

    #[tokio::test]
    async fn test_non_retryable_handler_error_does_not_crash_route() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("bad_input_in_{}", unique_id);
        let out_topic = format!("bad_input_out_{}", unique_id); // Not used, but good practice

        let input = Endpoint::new_memory(&in_topic, 10);
        let output = Endpoint::new_memory(&out_topic, 10);

        // A handler that fails on specific input
        let handler = |msg: crate::CanonicalMessage| async move {
            if msg.get_payload_str() == "poison" {
                Err(HandlerError::NonRetryable(anyhow::anyhow!("Invalid input")))
            } else {
                Ok(crate::Handled::Publish(msg))
            }
        };

        let route = Route::new(input.clone(), output).with_handler(handler);
        route.deploy("test_invalid_input").await.unwrap();

        let input_ch = input.channel().unwrap();
        let out_channel = route.output.channel().unwrap();

        // 1. Send poison message
        input_ch.send_message("poison".into()).await.unwrap();

        // 2. Send valid message
        input_ch.send_message("valid".into()).await.unwrap();

        // 3. Verify the valid message was processed and published
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(msg) = out_channel.drain_messages().pop() {
                    return msg;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Timed out waiting for valid message to be processed");
        assert_eq!(received.get_payload_str(), "valid");
        Route::stop("test_invalid_input").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_dlq_and_retry_batch_integration() {
        use crate::models::{DeadLetterQueueMiddleware, Middleware, RetryMiddleware};
        use crate::traits::{MessagePublisher, PublisherError, SentBatch};
        use std::collections::HashMap;
        use std::sync::Mutex;

        // Mock publisher that fails messages with even-numbered IDs
        #[derive(Clone)]
        struct PartialFailPublisher {
            attempts: Arc<Mutex<HashMap<u128, usize>>>,
        }

        #[async_trait::async_trait]
        impl MessagePublisher for PartialFailPublisher {
            async fn send_batch(
                &self,
                messages: Vec<CanonicalMessage>,
            ) -> Result<SentBatch, PublisherError> {
                let mut failed = Vec::new();
                let mut attempts = self.attempts.lock().unwrap();

                for msg in messages {
                    let msg_num: u32 = serde_json::from_slice::<serde_json::Value>(&msg.payload)
                        .unwrap()["id"]
                        .as_u64()
                        .unwrap() as u32;

                    let attempt_count = attempts.entry(msg.message_id).or_insert(0);
                    *attempt_count += 1;

                    if msg_num % 2 == 0 {
                        // Fail even numbers
                        failed.push((
                            msg,
                            PublisherError::Retryable(anyhow::anyhow!("simulated failure")),
                        ));
                    }
                    // Odd numbers succeed implicitly by not being in `failed`
                }

                if failed.is_empty() {
                    Ok(SentBatch::Ack)
                } else {
                    Ok(SentBatch::Partial {
                        responses: None,
                        failed,
                    })
                }
            }
            async fn send(
                &self,
                _msg: CanonicalMessage,
            ) -> Result<crate::traits::Sent, PublisherError> {
                unimplemented!()
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        // 1. Setup
        let in_topic = "batch_retry_dlq_in";
        let out_topic = "batch_retry_dlq_out";
        let dlq_topic = "batch_retry_dlq_dlq";

        let input = Endpoint::new_memory(in_topic, 10);
        let dlq_endpoint = Endpoint::new_memory(dlq_topic, 10);

        let mock_publisher = PartialFailPublisher {
            attempts: Arc::new(Mutex::new(HashMap::new())),
        };

        let mut output_with_middlewares = Endpoint::new_memory(out_topic, 10);
        output_with_middlewares.middlewares = vec![
            Middleware::Retry(RetryMiddleware {
                max_attempts: 2,
                initial_interval_ms: 1,
                ..Default::default()
            }),
            Middleware::Dlq(Box::new(DeadLetterQueueMiddleware {
                endpoint: dlq_endpoint.clone(),
            })),
        ];

        let route = Route::new(input.clone(), output_with_middlewares).with_batch_size(4);
        // Inject the mock publisher into the route's output
        let final_publisher = crate::middleware::apply_middlewares_to_publisher(
            Box::new(mock_publisher.clone()),
            &route.output,
            "test_route",
        )
        .await
        .unwrap();

        // We need a way to run the route with our mocked publisher.
        // The simplest way is to manually drive the core logic.
        let (work_tx, work_rx) =
            async_channel::bounded::<(Vec<crate::CanonicalMessage>, BatchCommitFunc)>(1);
        let (seq_tx, _sequencer_handle) = spawn_sequencer(1);

        // Spawn a worker to process one batch
        tokio::spawn(async move {
            if let Ok((messages, commit)) = work_rx.recv().await {
                let batch_len = messages.len();
                match final_publisher.send_batch(messages).await {
                    Ok(SentBatch::Ack) => {
                        let _ = commit(vec![MessageDisposition::Ack; batch_len]).await;
                    }
                    Ok(SentBatch::Partial { failed, .. }) => {
                        // In a real route, we'd map responses, but here we just care about failure.
                        let dispositions = if failed.is_empty() {
                            vec![MessageDisposition::Ack; batch_len]
                        } else {
                            // This is a simplification for the test. A real implementation
                            // would map dispositions based on message IDs.
                            vec![MessageDisposition::Nack; batch_len]
                        };
                        let _ = commit(dispositions).await;
                    }
                    Err(_) => {
                        let _ = commit(vec![MessageDisposition::Nack; batch_len]).await;
                    }
                }
            }
        });

        // 2. Send a batch of messages
        let mut messages = Vec::new();
        for i in 1..=4 {
            // 1 (ok), 2 (fail), 3 (ok), 4 (fail)
            messages.push(CanonicalMessage::from_json(serde_json::json!({"id": i})).unwrap());
        }
        let commit = wrap_commit(Box::new(|_| Box::pin(async { Ok(()) })), 0, seq_tx.clone());
        work_tx.send((messages, commit)).await.unwrap();

        // 3. Verify
        let dlq_channel = dlq_endpoint.channel().unwrap();

        let start = std::time::Instant::now();
        while dlq_channel.len() < 2 {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let dlq_msgs = dlq_channel.drain_messages();

        assert_eq!(dlq_msgs.len(), 2, "Expected 2 messages to go to DLQ");

        let dlq_ids: std::collections::HashSet<u32> = dlq_msgs
            .iter()
            .map(|m| {
                serde_json::from_slice::<serde_json::Value>(&m.payload).unwrap()["id"]
                    .as_u64()
                    .unwrap() as u32
            })
            .collect();

        assert!(dlq_ids.contains(&2));
        assert!(dlq_ids.contains(&4));

        // Verify retry attempts
        let attempts = mock_publisher.attempts.lock().unwrap();
        // Messages 2 and 4 should be tried `max_attempts` times.
        assert_eq!(attempts.values().filter(|&&c| c == 2).count(), 2);
        // Messages 1 and 3 should be tried once.
        assert_eq!(attempts.values().filter(|&&c| c == 1).count(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_route_dlq_integration() {
        // Setup: Input -> [Panic(Disconnect) -> Retry -> DLQ] -> Output
        // Panic(Disconnect) simulates transient failure.
        // Retry handles it up to N times.
        // If max attempts reached, DLQ catches it.
        // Note: Middleware application order is [Panic, Retry, DLQ] in list to wrap as DLQ(Retry(Panic(Endpoint))).

        let unique_id = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("dlq_in_{}", unique_id);
        let out_topic = format!("dlq_out_{}", unique_id);
        let dlq_topic = format!("dlq_target_{}", unique_id);

        let input = Endpoint::new_memory(&in_topic, 10);

        let dlq_endpoint = Endpoint::new_memory(&dlq_topic, 10);

        let mut output = Endpoint::new_memory(&out_topic, 10);
        output.middlewares = vec![
            // Inner-most: Fail always
            Middleware::RandomPanic(RandomPanicMiddleware {
                mode: FaultMode::Disconnect, // Returns Retryable error
                trigger_on_message: None,    // Fail always
                enabled: true,
                ..Default::default()
            }),
            // Middle: Retry
            Middleware::Retry(crate::models::RetryMiddleware {
                max_attempts: 2,
                initial_interval_ms: 10,
                max_interval_ms: 100,
                multiplier: 1.0,
            }),
            // Outer-most: DLQ
            Middleware::Dlq(Box::new(crate::models::DeadLetterQueueMiddleware {
                endpoint: dlq_endpoint.clone(),
            })),
        ];

        let route = Route::new(input.clone(), output);
        route.deploy("test_dlq_integration").await.unwrap();

        // Send message
        let input_ch = input.channel().unwrap();
        input_ch.send_message("fail_msg".into()).await.unwrap();

        // Verify:
        // 1. Output channel is empty (msg failed to go there)
        // 2. DLQ channel has message

        let dlq_ch = dlq_endpoint.channel().unwrap();

        // Wait for DLQ
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let batch = dlq_ch.drain_messages();
                if !batch.is_empty() {
                    return batch[0].clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Timed out waiting for DLQ");

        assert_eq!(received.get_payload_str(), "fail_msg");

        let out_ch_target = mq_bridge::endpoints::memory::get_or_create_channel(
            &mq_bridge::models::MemoryConfig::new(&out_topic, None),
        );
        assert!(out_ch_target.is_empty(), "Message should not reach target");

        Route::stop("test_dlq_integration").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_large_message_handling() {
        let unique_id = fast_uuid_v7::gen_id().to_string();
        let in_topic = format!("large_in_{}", unique_id);
        let out_topic = format!("large_out_{}", unique_id);

        let input = Endpoint::new_memory(&in_topic, 5); // Small capacity
        let output = Endpoint::new_memory(&out_topic, 5);

        let route = Route::new(input.clone(), output.clone());
        route.deploy("test_large_msg").await.unwrap();

        let large_payload = vec![b'x'; 5 * 1024 * 1024]; // 5MB
        let input_ch = input.channel().unwrap();

        input_ch
            .send_message(large_payload.clone().into())
            .await
            .unwrap();

        let mut verifier = route.connect_to_output("verifier").await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), verifier.receive())
            .await
            .expect("Timed out receiving large message")
            .unwrap();

        assert_eq!(received.message.payload.len(), large_payload.len());
        assert_eq!(received.message.payload, large_payload.as_slice());

        Route::stop("test_large_msg").await;
    }

    #[test]
    fn test_map_responses_to_dispositions_unit() {
        test_map_responses_to_dispositions_logic();
    }
}
