use std::sync::{atomic, Arc};
//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue
use crate::endpoints::{create_consumer_from_route, create_publisher_from_route};
pub use crate::models::Route;
use crate::traits::CommitFunc;
use async_channel::{bounded, Sender};
use tokio::{
    select,
    task::{self, JoinHandle},
};
use tracing::{debug, error, info, warn};

impl Route {
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
            let running = Arc::new(atomic::AtomicBool::new(true));
            loop {
                let running_clone = Arc::clone(&running);
                // cheap pointer clones
                let route_arc = Arc::clone(&route);
                let name_arc = Arc::clone(&name);

                let mut run_task = tokio::spawn(async move {
                    route_arc
                        .run_until_err(&name_arc, Some(running_clone))
                        .await
                });

                select! {
                    res = shutdown_rx.recv() => {
                        if res.is_err() {
                            warn!("Shutdown channel for route '{}' closed unexpectedly.", name);
                        }
                        info!("Shutdown signal received for route '{}'.", name);
                        running.store(false, atomic::Ordering::Relaxed);
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
                            _ => {}
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
        running: Option<Arc<atomic::AtomicBool>>,
    ) -> anyhow::Result<bool> {
        if self.concurrency == 1 {
            self.run_sequentially(name, running).await
        } else {
            self.run_concurrently(name, running).await
        }
    }

    /// A simplified, sequential runner for when concurrency is 1.
    async fn run_sequentially(
        &self,
        name: &str,
        running: Option<Arc<atomic::AtomicBool>>,
    ) -> anyhow::Result<bool> {
        let running = if let Some(running) = running {
            running
        } else {
            Arc::new(atomic::AtomicBool::new(true))
        };
        let publisher =
            Arc::new(create_publisher_from_route(name, &self.output.endpoint_type).await?);
        let mut consumer = create_consumer_from_route(name, &self.input.endpoint_type).await?;

        while running.load(atomic::Ordering::Relaxed) {
            let (message, commit) = match consumer.receive().await {
                Ok(val) => val,
                Err(e) => {
                    warn!("Consumer returned an error, likely end-of-stream: {}. Shutting down route.", e);
                    break; // Graceful exit on end-of-stream
                }
            };

            // Process sequentially without spawning a new task
            match publisher.send(message).await {
                Ok(response) => commit(response).await,
                Err(e) => error!("Failed to send message in sequential route: {}", e),
            }
        }
        Ok(running.load(atomic::Ordering::Relaxed))
    }

    /// The main concurrent runner for when concurrency > 1.
    async fn run_concurrently(
        &self,
        name: &str,
        running: Option<Arc<atomic::AtomicBool>>,
    ) -> anyhow::Result<bool> {
        let running = if let Some(running) = running {
            running
        } else {
            Arc::new(atomic::AtomicBool::new(true))
        };
        let publisher =
            Arc::new(create_publisher_from_route(name, &self.output.endpoint_type).await?);
        let mut consumer = create_consumer_from_route(name, &self.input.endpoint_type).await?;
        let (err_tx, err_rx) = bounded(1); // For critical, route-stopping errors
                                           // channel capacity: a small buffer proportional to concurrency
        let work_capacity = self.concurrency.saturating_mul(4);
        let (work_tx, work_rx) = bounded::<(crate::CanonicalMessage, CommitFunc)>(work_capacity);

        // --- Worker Pool ---
        let mut worker_handles = Vec::with_capacity(self.concurrency);
        for i in 0..self.concurrency {
            let work_rx_clone = work_rx.clone();
            let publisher = Arc::clone(&publisher);
            let err_tx = err_tx.clone();
            worker_handles.push(task::spawn(async move {
                debug!("Starting worker {}", i);
                while let Ok((message, commit)) = work_rx_clone.recv().await {
                    match publisher.send(message).await {
                        Ok(response) => commit(response).await,
                        Err(e) => {
                            error!("Worker failed to send message: {}", e);
                            // Send the error back to the main task to tear down the route
                            if err_tx.send(e).await.is_err() {
                                warn!("Could not send error to main task, it might be down.");
                            }
                        }
                    }
                }
            }));
        }

        while running.load(atomic::Ordering::Relaxed) {
            select! {
                biased; // Prioritize checking for errors

                Ok(err) = err_rx.recv() => {
                    error!("A worker reported a critical error. Shutting down route.");
                    return Err(err);
                }

                res = consumer.receive() => {
                    let (message, commit) = match res {
                        Ok(val) => val,
                        Err(e) => {
                            warn!("Consumer returned an error, likely end-of-stream: {}. Shutting down route.", e);
                            break; // Exit the select loop gracefully
                        }
                    };
                    if work_tx.send((message, commit)).await.is_err() {
                        warn!("Work channel closed, cannot process more messages. Shutting down.");
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
        Ok(running.load(atomic::Ordering::Relaxed))
    }
}
