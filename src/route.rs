//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::endpoints::{create_consumer_from_route, create_publisher_from_route};
pub use crate::models::Route;
use crate::models::{self, Endpoint};
use crate::traits::{
    BatchCommitFunc, ConsumerError, Handler, HandlerError, PublisherError, SentBatch,
};
use async_channel::{bounded, Sender};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::sync::Arc;
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
        let _ = self.0.1.send(()).await;
    }

    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        self.0.0.await
    }
}

impl From<(JoinHandle<()>, Sender<()>)> for RouteHandle {
    fn from(tuple: (JoinHandle<()>, Sender<()>)) -> Self {
        RouteHandle(tuple)
    }
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
    /// Runs the message processing route with concurrency, error handling, and graceful shutdown.
    ///
    /// This function spawns a set of worker tasks to process messages concurrently.
    /// It returns a `JoinHandle` for the main route task and a `Sender` channel
    /// that can be used to signal a graceful shutdown.
    pub fn run(&self, name_str: &str) -> anyhow::Result<(JoinHandle<()>, Sender<()>)> {
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

        let ready_rx_clone = ready_rx.clone();
        let timeout = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            ready_rx_clone.close();
        });

        match ready_rx.recv_blocking() {
            Ok(_) => {
                timeout.abort();
                Ok((handle, shutdown_tx))
            }
            Err(_) => {
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
        if let Some(tx) = ready_tx {
            let _ = tx.send(()).await;
        }
        loop {
            select! {
                Ok(err) = err_rx.recv() => return Err(err),

                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in sequential runner for route '{}'.", name);
                    while commit_tasks.join_next().await.is_some() {}
                    return Ok(true); // Stopped by shutdown signal
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
                            break; // Graceful exit
                        }
                        Err(ConsumerError::Connection(e)) => {
                            // Propagate error to trigger reconnect by the outer loop
                            return Err(e);
                        }
                    };
                    debug!("Received a batch of {} messages sequentially", received_batch.messages.len());

                    // Process the batch sequentially without spawning a new task
                    let commit = received_batch.commit;
                    match publisher.send_batch(received_batch.messages).await {
                        Ok(SentBatch::Ack) => {
                            let permit = commit_semaphore.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;
                            let err_tx = err_tx.clone();
                            commit_tasks.spawn(async move {
                                if let Err(e) = commit(None).await {
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
                                return Err(anyhow::anyhow!(
                                    "Failed to send {} messages in batch. First retryable error: {}",
                                    failed_count,
                                    first_error
                                ));
                            }
                            for (msg, e) in failed {
                                error!("Dropping message (ID: {:032x}) due to non-retryable error: {}", msg.message_id, e);
                            }
                            let permit = commit_semaphore.clone().acquire_owned().await.map_err(|e| anyhow::anyhow!("Semaphore error: {}", e))?;
                            let err_tx = err_tx.clone();
                            commit_tasks.spawn(async move {
                                if let Err(e) = commit(responses).await {
                                    error!("Commit failed: {}", e);
                                    let _ = err_tx.send(e).await;
                                }
                                drop(permit);
                            });
                        }
                        Err(e) => return Err(e.into()), // Propagate error to trigger reconnect
                    }
                }
            }
        }
        while commit_tasks.join_next().await.is_some() {}
        Ok(false) // Indicate graceful shutdown due to end-of-stream
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
        type SequencerItem = (
            Option<Vec<crate::CanonicalMessage>>,
            BatchCommitFunc,
            tokio::sync::oneshot::Sender<anyhow::Result<()>>,
        );
        let (seq_tx, seq_rx) = bounded::<(u64, SequencerItem)>(self.concurrency * 2);

        let sequencer_handle = tokio::spawn(async move {
            let mut buffer: BTreeMap<u64, SequencerItem> = BTreeMap::new();
            let mut next_seq = 0u64;
            while let Ok((seq, item)) = seq_rx.recv().await {
                buffer.insert(seq, item);
                while let Some((responses, commit_func, notify)) = buffer.remove(&next_seq) {
                    // Execute the commit
                    let res = commit_func(responses).await;
                    // Notify the worker that commit is done
                    let _ = notify.send(res);
                    next_seq += 1;
                }
            }
        });

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
                                if let Err(e) = commit(None).await {
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
                            for (msg, e) in failed {
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
                                if let Err(e) = commit(responses).await {
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
                    let seq_tx = seq_tx.clone();

                    let wrapped_commit: BatchCommitFunc = Box::new(move |responses| {
                        Box::pin(async move {
                            let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
                            // Send to sequencer
                            if seq_tx.send((seq, (responses, commit, notify_tx))).await.is_ok() {
                                // Wait for sequencer to execute the commit
                                match notify_rx.await {
                                    Ok(res) => res,
                                    Err(_) => {
                                        Err(anyhow::anyhow!("Sequencer dropped the commit channel unexpectedly"))
                                    }
                                }
                            } else {
                                Err(anyhow::anyhow!("Failed to send commit to sequencer, route is likely shutting down"))
                            }
                        })
                    });

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
        let unique_suffix = uuid::Uuid::now_v7().simple().to_string();
        let in_topic = format!("panic_in_{}", unique_suffix);
        let out_topic = format!("panic_out_{}", unique_suffix);

        let should_panic = Arc::new(AtomicBool::new(true));
        let factory = PanicMiddlewareFactory {
            should_panic: should_panic.clone(),
        };

        let input = Endpoint::new_memory(&in_topic, 10)
            .add_middleware(Middleware::Custom(Arc::new(factory)));
        let output = Endpoint::new_memory(&out_topic, 10);

        let route = Route::new(input.clone(), output.clone());

        // Start the route
        let route_handle = RouteHandle(route.run("panic_test").expect("Failed to run route"));
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
        let output_ch = output.channel().unwrap();
        let mut received = Vec::new();

        // Poll for output for up to 10 seconds
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(10) {
            let msgs = output_ch.drain_messages();
            if !msgs.is_empty() {
                received.extend(msgs);
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        assert_eq!(
            received.len(),
            1,
            "Should have received the message after recovery"
        );
        assert_eq!(received[0].get_payload_str(), "persistent_msg");

        // Cleanup
        route_handle.stop().await;
        let _ = route_handle.join().await;
    }
}
