use crate::models::{self, Endpoint};
use std::sync::Arc;
//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge
pub use crate::models::Route;
use crate::traits::{BatchCommitFunc, ConsumerError, SentBatch};
use crate::{
    endpoints::{create_consumer_from_route, create_publisher_from_route},
    traits::Handler,
};
use async_channel::{bounded, Sender};
use tokio::{
    select,
    task::{self, JoinHandle},
};
use tracing::{debug, error, info, warn};

impl Route {
    pub fn new(input: Endpoint, output: Endpoint) -> Self {
        Self {
            input,
            output,
            concurrency: models::default_concurrency(),
            batch_size: models::default_batch_size(),
        }
    }
    /// Runs the message processing route with concurrency, error handling, and graceful shutdown.
    ///
    /// This function spawns a set of worker tasks to process messages concurrently.
    /// It returns a `JoinHandle` for the main route task and a `Sender` channel
    /// that can be used to signal a graceful shutdown.
    pub fn run(&self, name: &str) -> (JoinHandle<()>, Sender<()>) {
        let (shutdown_tx, shutdown_rx) = bounded(1);
        // Use `Arc` so route/name clones are cheap (pointer copy) in the reconnect loop.
        let route = Arc::new(self.clone());
        let name = Arc::new(name.to_string());

        let handle = tokio::spawn(async move {
            loop {
                let route_arc = Arc::clone(&route);
                let name_arc = Arc::clone(&name);
                // Create a new, per-iteration internal shutdown channel.
                // This avoids a race where both this loop and the inner task
                // try to consume the same external shutdown signal.
                let (internal_shutdown_tx, internal_shutdown_rx) = bounded(1);

                // The actual route logic is in `run_until_err`.
                let mut run_task = tokio::spawn(async move {
                    route_arc
                        .run_until_err(&name_arc, Some(internal_shutdown_rx))
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

        (handle, shutdown_tx)
    }

    /// The core logic of running the route, designed to be called within a reconnect loop.
    pub async fn run_until_err(
        &self,
        name: &str,
        shutdown_rx: Option<async_channel::Receiver<()>>,
    ) -> anyhow::Result<bool> {
        let (_internal_shutdown_tx, internal_shutdown_rx) = bounded(1);
        let shutdown_rx = shutdown_rx.unwrap_or(internal_shutdown_rx);
        if self.concurrency == 1 {
            self.run_sequentially(name, shutdown_rx).await
        } else {
            self.run_concurrently(name, shutdown_rx).await
        }
    }

    /// A simplified, sequential runner for when concurrency is 1.
    async fn run_sequentially(
        &self,
        name: &str,
        shutdown_rx: async_channel::Receiver<()>,
    ) -> anyhow::Result<bool> {
        let publisher = create_publisher_from_route(name, &self.output).await?;
        let mut consumer = create_consumer_from_route(name, &self.input).await?;

        loop {
            select! {
                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in sequential runner for route '{}'.", name);
                    return Ok(true); // Stopped by shutdown signal
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
                            return Err(e.into());
                        }
                    };
                    debug!("Received a batch of {} messages sequentially", messages.len());

                    // Process the batch sequentially without spawning a new task
                    match publisher.send_batch(messages).await {
                        Ok(SentBatch::Ack) => {
                            commit(None).await;
                        }
                        Ok(SentBatch::Partial { responses, failed }) => {
                            let failed_count = failed.len();
                            commit(responses).await; // Commit the successful messages
                            if failed_count > 0 {
                                // Replicate old behavior: any failure in a batch is a route-level error.
                                return Err(anyhow::anyhow!(
                                    "Failed to send {} messages in batch (non-retryable).",
                                    failed_count
                                ));
                            }
                        }
                        Err(e) => return Err(e.into()), // Propagate error to trigger reconnect
                    }
                }
            }
        }
        Ok(false) // Indicate graceful shutdown due to end-of-stream
    }

    /// The main concurrent runner for when concurrency > 1.
    async fn run_concurrently(
        &self,
        name: &str,
        shutdown_rx: async_channel::Receiver<()>,
    ) -> anyhow::Result<bool> {
        let publisher = create_publisher_from_route(name, &self.output).await?;
        let mut consumer = create_consumer_from_route(name, &self.input).await?;
        let (err_tx, err_rx) = bounded(1); // For critical, route-stopping errors
                                           // channel capacity: a small buffer proportional to concurrency
        let work_capacity = self.concurrency.saturating_mul(self.batch_size);
        let (work_tx, work_rx) =
            bounded::<(Vec<crate::CanonicalMessage>, BatchCommitFunc)>(work_capacity);

        // --- Worker Pool ---
        let mut worker_handles = Vec::with_capacity(self.concurrency);
        for i in 0..self.concurrency {
            let work_rx_clone = work_rx.clone();
            let publisher = Arc::clone(&publisher);
            let err_tx = err_tx.clone();
            worker_handles.push(task::spawn(async move {
                debug!("Starting worker {}", i);
                while let Ok((messages, commit)) = work_rx_clone.recv().await {
                    // The worker now receives a batch and sends it as a bulk.
                    match publisher.send_batch(messages).await {
                        Ok(SentBatch::Ack) => {
                            commit(None).await;
                        }
                        Ok(SentBatch::Partial { responses, failed }) => {
                            let failed_count = failed.len();
                            commit(responses).await; // Commit the successful messages
                            if failed_count > 0 {
                                let e = anyhow::anyhow!(
                                    "Failed to send {} messages in batch (non-retryable).",
                                    failed_count
                                );
                                error!("Worker failed to send message batch: {}", e);
                                if err_tx.send(e).await.is_err() {
                                    warn!("Could not send error to main task, it might be down.");
                                }
                            }
                        }
                        Err(e) => {
                            error!("Worker failed to send message batch: {}", e);
                            // Send the error back to the main task to tear down the route.
                            if err_tx.send(e.into()).await.is_err() {
                                warn!("Could not send error to main task, it might be down.");
                            }
                        }
                    }
                }
            }));
        }

        loop {
            select! {
                biased; // Prioritize checking for errors

                Ok(err) = err_rx.recv() => {
                    error!("A worker reported a critical error. Shutting down route.");
                    return Err(err);
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
                            return Err(e.into());
                        }
                    };
                    debug!("Received a batch of {} messages concurrently", messages.len());
                    if work_tx.send((messages, commit)).await.is_err() {
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
        for handle in worker_handles {
            let _ = handle.await;
        }
        // Return true if we should continue (i.e., we were stopped by the running flag), false otherwise.
        Ok(shutdown_rx.is_empty())
    }

    pub fn with_handler(mut self, handler: Arc<dyn Handler>) -> Self {
        self.output.handler = Some(handler);
        self
    }
}
