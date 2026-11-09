//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The run loops: start, reconnect, sequential and concurrent execution.

use super::*;

impl Route {
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
}

impl Route {
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
        self.run_with_resume_policy(name_str, false).await
    }

    /// Runs the route without optional cursor/checkpoint resume state.
    ///
    /// This is intended for deliberate full copies. It suppresses resume setup,
    /// warnings, and errors for cursor-based sources without changing native
    /// broker offsets or other consumption semantics.
    pub async fn run_without_resume(&self, name_str: &str) -> anyhow::Result<RouteHandle> {
        self.run_with_resume_policy(name_str, true).await
    }

    pub(super) async fn run_with_resume_policy(
        &self,
        name_str: &str,
        no_resume: bool,
    ) -> anyhow::Result<RouteHandle> {
        let shown = Arc::new(route_display_name(name_str));
        // The checks take the key; only their text shows the display name.
        let warnings = self
            .check_with_resume(name_str, None, no_resume)
            .map_err(|e| show_route_error(e, name_str, &shown))?;
        for warning in warnings {
            let warning = show_route_name(warning, name_str, &shown);
            tracing::warn!(route = %shown, "Configuration warning: {}", warning);
        }
        let relaxed_output = relax_object_naming(
            name_str,
            self.source_has_position(),
            &self.input,
            &self.output,
        )
        .map_err(|e| show_route_error(e, name_str, &shown))?
        .map(|(output, _)| output);
        let inferred_output = relaxed_output.as_ref().unwrap_or(&self.output);
        match self.inferred_delivery(inferred_output) {
            (delivery, Some(mechanism)) => tracing::info!(
                route = %shown,
                delivery = %delivery,
                mechanism,
                "Inferred delivery guarantee from idempotent sink configuration"
            ),
            (delivery, None) => tracing::info!(
                route = %shown,
                delivery = %delivery,
                "Inferred delivery guarantee"
            ),
        }
        let startup_timeout = std::time::Duration::from_millis(self.options.startup_timeout_ms);
        let reconnect_interval =
            tokio::time::Duration::from_millis(self.options.reconnect_interval_ms);
        let (shutdown_tx, shutdown_rx) = bounded(1);
        let (ready_tx, ready_rx) = bounded(1);
        // Use `Arc` so route/name clones are cheap (pointer copy) in the reconnect loop.
        let route = Arc::new(self.clone());
        let name = Arc::new(name_str.to_string());
        let shown_loop = Arc::clone(&shown);
        // Errors embed the route name (`[route:…]`); print the display name there too.
        let show = {
            let name = Arc::clone(&name);
            let shown = Arc::clone(&shown);
            move |text: String| show_route_name(text, &name, &shown)
        };

        // Live health cell shared with the returned `RouteHandle`. Starts unhealthy
        // ("connecting") and is flipped to healthy once the route reports ready, back
        // to unhealthy (with the last error) whenever the run task fails.
        let status = Arc::new(RwLock::new(EndpointStatus {
            healthy: false,
            target: name_str.to_string(),
            error: Some("connecting".to_string()),
            ..Default::default()
        }));
        let status_loop = Arc::clone(&status);
        // Terminal outcome cell, published by `OutcomeGuard` as the task exits.
        let outcome = Arc::new(RwLock::new(None::<RouteOutcome>));
        // Tally of discarded messages, published alongside the terminal outcome.
        let drops = Arc::new(RwLock::new(DropReport::default()));
        let sends = Arc::new(PendingSends::new(self.options.concurrency));
        let sends_loop = Arc::clone(&sends);
        let mut outcome_guard = OutcomeGuard {
            outcome: Arc::clone(&outcome),
            status: Arc::clone(&status),
            drops: Arc::clone(&drops),
            resolved: None,
        };

        let exit_on_empty = self.options.exit_on_empty;
        let handle = tokio::spawn(async move {
            // The startup `ready` channel is consumed once by `run()`; only the first
            // (re)connect needs to notify it.
            let mut startup_notified = false;
            // Reconnects since the route last ran stably. Drives both the drain-mode
            // bound below and the health cell: a route that reconnects on a loop is
            // flapping, and reporting it `healthy` the moment it connects again hides
            // exactly the failure an operator is looking for.
            let mut consecutive_failures = 0usize;
            let run_positions = RunPositions::default();
            'reconnect: loop {
                let route_arc = Arc::clone(&route);
                let name_arc = Arc::clone(&name);
                // Create a new, per-iteration internal shutdown channel.
                // This avoids a race where both this loop and the inner task
                // try to consume the same external shutdown signal.
                let (internal_shutdown_tx, internal_shutdown_rx) = bounded(1);
                // Per-iteration ready channel so we can observe every (re)connection,
                // not just the first startup, and update the shared health cell.
                let (iter_ready_tx, iter_ready_rx) = bounded(1);

                // The actual route logic is in `run_until_err`.
                let drops_run = Arc::clone(&drops);
                let sends_run = Arc::clone(&sends_loop);
                let positions = Arc::clone(&run_positions);
                let mut run_task = tokio::spawn(RUN_POSITIONS.scope(positions, async move {
                    route_arc
                        .run_until_err_reporting_to(
                            &name_arc,
                            Some(internal_shutdown_rx),
                            Some(iter_ready_tx),
                            Some(&drops_run),
                            Some(&sends_run),
                            no_resume,
                        )
                        .await
                }));

                // Inner loop: process ready + result events for this connection attempt.
                loop {
                    select! {
                        _ = shutdown_rx.recv() => {
                            info!("Shutdown signal received for route '{}'.", shown_loop);
                            // Notify the inner task to shut down.
                            let _ = internal_shutdown_tx.send(()).await;
                            // Wait for the inner task to finish gracefully.
                            let _ = run_task.await;
                            forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
                            outcome_guard.set(RouteOutcome::Stopped);
                            break 'reconnect;
                        }
                        // A pass that stays up this long has recovered: clear the flap
                        // state and the stale error. Disabled while healthy, so a route
                        // that never fails never arms the timer.
                        _ = tokio::time::sleep(STABLE_RUN), if consecutive_failures > 0 => {
                            consecutive_failures = 0;
                            let mut s = recover_write_lock(&status_loop, "route_handle_status");
                            s.healthy = true;
                            s.error = None;
                        }
                        Ok(_) = iter_ready_rx.recv() => {
                            // The route (re)connected and is ready to process messages.
                            // Connected is not the same as healthy while it is still
                            // failing on every pass: keep the last error visible until the
                            // pass above proves it stayed up.
                            {
                                let mut s = recover_write_lock(&status_loop, "route_handle_status");
                                if consecutive_failures == 0 {
                                    s.healthy = true;
                                    s.error = None;
                                }
                            }
                            // Forward to the startup `ready` channel so `run()` can return.
                            if !startup_notified {
                                let _ = ready_tx.send(()).await;
                                startup_notified = true;
                            }
                            // Keep waiting for the run result / shutdown.
                        }
                        res = &mut run_task => {
                            forward_buffered_ready(&mut startup_notified, &iter_ready_rx, &ready_tx).await;
                            match res {
                                Ok(Ok(should_continue)) if !should_continue => {
                                    info!("Route '{}' completed gracefully. Shutting down.", shown_loop);
                                    outcome_guard.set(RouteOutcome::Completed);
                                    break 'reconnect;
                                }
                                Ok(Err(e)) => {
                                    // An exhausted source is a clean terminal, not a failure:
                                    // it ends the route the same way `exit_on_empty` does.
                                    let is_end_of_stream = e.downcast_ref::<ConsumerError>().is_some_and(|ce| matches!(ce, ConsumerError::EndOfStream));
                                    let is_permanent =
                                        e.downcast_ref::<ProcessingError>().is_some_and(|pe| matches!(pe, ProcessingError::NonRetryable(_)))
                                        || e.downcast_ref::<ConsumerError>().is_some_and(|ce| matches!(ce, ConsumerError::Permanent(_)))
                                        || e.is::<crate::errors::InvalidConfig>()
                                        || (exit_on_empty && is_authentication_failure(&e))
                                        || is_end_of_stream;

                                    // EndOfStream is a clean terminal, not a failure, so
                                    // it must not mark the route unhealthy.
                                    if !is_end_of_stream {
                                        let mut s = recover_write_lock(&status_loop, "route_handle_status");
                                        s.healthy = false;
                                        s.error = Some(show(e.to_string()));
                                        consecutive_failures += 1;
                                    }

                                    // A drain job is finite: if it cannot get past the same
                                    // failure it will never reach the empty batch it is
                                    // waiting for, so bound it instead of retrying forever.
                                    // A continuous route is meant to reconnect indefinitely.
                                    let delivered_anything = sends_loop.delivered.load(Ordering::Relaxed);
                                    if !is_permanent
                                        && exit_on_empty
                                        && drain_gives_up(delivered_anything, consecutive_failures)
                                    {
                                        outcome_guard.set(RouteOutcome::Failed);
                                        if delivered_anything {
                                            error!(
                                                "Route '{}' failed {} times in a row while draining; giving up. Last error: {}",
                                                shown_loop, consecutive_failures, show(e.to_string())
                                            );
                                        } else {
                                            error!(
                                                "Route '{}' failed before it delivered anything; a drain does not wait for an endpoint to become available. Error: {}",
                                                shown_loop, show(e.to_string())
                                            );
                                        }
                                        break 'reconnect;
                                    }

                                    if is_permanent {
                                        if is_end_of_stream {
                                            outcome_guard.set(RouteOutcome::Completed);
                                            info!("Route '{}' completed: end of stream. Shutting down.", shown_loop);
                                        } else {
                                            outcome_guard.set(RouteOutcome::Failed);
                                            error!("Route '{}' failed with a permanent error: {}. Shutting down.", shown_loop, show(e.to_string()));
                                        }
                                        break 'reconnect;
                                    }

                                    warn!(
                                        "Route '{}' failed: {}. Reconnecting in {}ms...",
                                        shown_loop,
                                        show(e.to_string()),
                                        reconnect_interval.as_millis()
                                    );
                                    if !reconnect_interval.is_zero() {
                                        select! {
                                            _ = tokio::time::sleep(reconnect_interval) => {}
                                            _ = shutdown_rx.recv() => {
                                                info!("Shutdown signal received for route '{}'.", shown_loop);
                                                outcome_guard.set(RouteOutcome::Stopped);
                                                break 'reconnect;
                                            }
                                        }
                                    }
                                    break; // -> next reconnect iteration
                                }
                                Err(e) => {
                                    {
                                        let mut s = recover_write_lock(&status_loop, "route_handle_status");
                                        s.healthy = false;
                                        s.error = Some(format!("route task panicked: {}", e));
                                        consecutive_failures += 1;
                                    }
                                    // Same bound as the error arm: a drain whose task keeps
                                    // panicking will never reach its empty batch.
                                    if exit_on_empty
                                        && drain_gives_up(
                                            sends_loop.delivered.load(Ordering::Relaxed),
                                            consecutive_failures,
                                        )
                                    {
                                        outcome_guard.set(RouteOutcome::Failed);
                                        error!(
                                            "Route '{}' panicked {} times in a row while draining; giving up. Last error: {}",
                                            shown_loop, consecutive_failures, e
                                        );
                                        break 'reconnect;
                                    }
                                    error!(
                                        "Route '{}' task panicked: {}. Reconnecting in {}ms...",
                                        shown_loop,
                                        e,
                                        reconnect_interval.as_millis()
                                    );
                                    if !reconnect_interval.is_zero() {
                                        select! {
                                            _ = tokio::time::sleep(reconnect_interval) => {}
                                            _ = shutdown_rx.recv() => {
                                                info!("Shutdown signal received for route '{}'.", shown_loop);
                                                outcome_guard.set(RouteOutcome::Stopped);
                                                break 'reconnect;
                                            }
                                        }
                                    }
                                    break; // -> next reconnect iteration
                                }
                                _ => break, // The route should reconnect and continue running.
                            }
                        }
                    }
                }
            }
        });

        let started = tokio::time::timeout(startup_timeout, ready_rx.recv()).await;
        if let Ok(Ok(_)) = started {
            return Ok(RouteHandle {
                handle,
                shutdown_tx,
                status,
                outcome,
                sends: Some(sends),
            });
        }
        // The startup failure itself stays inside the reconnect loop, so
        // surface the cause it recorded rather than a bare timeout.
        // "connecting" is the initial marker, not a recorded failure.
        //
        // Read before aborting: the abort drops `OutcomeGuard`, which replaces
        // the recorded cause with its panicked-or-aborted fallback as soon as
        // the task is polled. Under load that lands first and the real reason
        // — an unusable filter expression, say — is lost.
        let cause = recover_read_lock(&status, "route_handle_status")
            .error
            .clone()
            .filter(|e| e != "connecting")
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        handle.abort();
        Err(match started {
            // Every `ready_tx` was dropped: the route task ended without ever
            // signalling ready. That returns immediately, so calling it a timeout
            // sends the reader after a stall that never happened.
            Ok(Err(_)) => {
                let terminal = *recover_read_lock(&outcome, "route_handle_outcome");
                anyhow::anyhow!(
                    "Route '{}' failed to start: ended before it signalled ready (outcome: {}){}",
                    shown,
                    terminal.map_or("unknown", |o| match o {
                        RouteOutcome::Completed => "completed",
                        RouteOutcome::Stopped => "stopped",
                        RouteOutcome::Failed => "failed",
                    }),
                    cause
                )
            }
            _ => anyhow::anyhow!(
                "Route '{}' failed to start: did not become ready within {}ms{}",
                shown,
                startup_timeout.as_millis(),
                if cause.is_empty() {
                    ": an endpoint was still connecting and had reported no error"
                } else {
                    cause.as_str()
                }
            ),
        })
    }

    /// The core logic of running the route, designed to be called within a reconnect loop.
    pub async fn run_until_err(
        &self,
        name: &str,
        shutdown_rx: Option<async_channel::Receiver<()>>,
        ready_tx: Option<Sender<()>>,
    ) -> anyhow::Result<bool> {
        self.run_until_err_reporting_to(name, shutdown_rx, ready_tx, None, None, false)
            .await
    }

    /// [`run_until_err`](Self::run_until_err) with somewhere to report dropped
    /// messages. A deployed route passes its status cell so that a permanent
    /// sink rejection is still visible after the route completes; a caller that
    /// drives a `Route` directly has no such cell and passes `None`.
    pub(crate) async fn run_until_err_reporting_to(
        &self,
        name: &str,
        shutdown_rx: Option<async_channel::Receiver<()>>,
        ready_tx: Option<Sender<()>>,
        drops: Option<&Arc<RwLock<DropReport>>>,
        sends: Option<&Arc<PendingSends>>,
        no_resume: bool,
    ) -> anyhow::Result<bool> {
        let (_internal_shutdown_tx, internal_shutdown_rx) = bounded(1);
        let shutdown_rx = shutdown_rx.unwrap_or(internal_shutdown_rx);
        crate::endpoints::check_distinct_files(name, &self.input, &self.output)?;
        if let Some(result) = crate::endpoints::try_run_fast_path_route(
            self,
            name,
            shutdown_rx.clone(),
            ready_tx.clone(),
        )
        .await
        {
            return result;
        }
        if self.options.concurrency == 1 {
            self.run_sequentially(name, shutdown_rx, ready_tx, drops, sends, no_resume)
                .await
        } else {
            self.run_concurrently(name, shutdown_rx, ready_tx, drops, sends, no_resume)
                .await
        }
    }

    /// A simplified, sequential runner for when concurrency is 1.
    pub(super) async fn run_sequentially(
        &self,
        name: &str,
        shutdown_rx: async_channel::Receiver<()>,
        ready_tx: Option<Sender<()>>,
        drops: Option<&Arc<RwLock<DropReport>>>,
        sends: Option<&Arc<PendingSends>>,
        no_resume: bool,
    ) -> anyhow::Result<bool> {
        let source_has_position = self.source_has_position();
        let relaxed_output;
        let output =
            match relax_object_naming(name, source_has_position, &self.input, &self.output)? {
                Some((endpoint, reason)) => {
                    warn!(route = name, "{reason}");
                    relaxed_output = endpoint;
                    &relaxed_output
                }
                None => &self.output,
            };
        let synced_output;
        let output = match sync_file_sinks_behind_saved_position(
            name,
            &self.input,
            output,
            source_has_position,
            no_resume,
        ) {
            Some(endpoint) => {
                synced_output = endpoint;
                &synced_output
            }
            None => output,
        };
        let unsynced_output;
        let output = match relax_object_store_fsync(
            name,
            &self.input,
            output,
            source_has_position,
            no_resume,
        ) {
            Some(endpoint) => {
                unsynced_output = endpoint;
                &unsynced_output
            }
            None => output,
        };
        let source_metadata_required =
            output_requires_source_metadata(name, output, source_has_position)?;
        check_source_position_available(name, &self.input, source_metadata_required)?;
        let source_metadata_required = source_metadata_required
            || (source_has_position && output_keys_on_source_position(name, output)?);
        let publisher =
            create_publisher_from_route_with_source_position(name, output, source_has_position)
                .await?;
        let transient_failure_policy = transient_publish_failure_policy(
            &self.input,
            output_passes_through_http_status(name, output)?,
        );
        let mut consumer = create_consumer_from_route_with_policy(
            name,
            &self.input,
            source_metadata_required,
            no_resume,
        )
        .await?;
        consumer.set_exit_on_empty(self.options.exit_on_empty);
        if let Err(err) = run_publisher_connect_hook(name, &publisher).await {
            run_publisher_disconnect_hook(name, &publisher, DisconnectOutcome::Failed).await;
            return Err(err);
        }
        if let Err(err) = run_consumer_connect_hook(name, consumer.as_ref()).await {
            run_consumer_disconnect_hook(name, consumer.as_ref()).await;
            run_publisher_disconnect_hook(name, &publisher, DisconnectOutcome::Failed).await;
            return Err(err);
        }
        let (err_tx, err_rx) = bounded(1);
        let commit_tasks = JoinSet::new();

        // Ordered consumers (cumulative-ack) commit through the serial sequencer;
        // individual-ack consumers commit concurrently under a semaphore.
        let commit_router = CommitRouter::new(
            consumer.commit_requires_order(),
            self.options.commit_concurrency_limit,
            // The batch being sent and the one read ahead.
            2,
        );
        let commit_semaphore = commit_router.dispatch_semaphore();
        let read_semaphore = commit_router.read_semaphore();
        let mut read_permit = None;
        let mut seq_counter = 0u64;
        let flush_gate = commit_router.flush_gate(&publisher, &err_tx);

        if let Some(tx) = ready_tx {
            let _ = tx.send(()).await;
        }
        let send_context = Arc::new(SendContext {
            publisher: Arc::clone(&publisher),
            has_retry_middleware: self.output_has_middleware(|m| matches!(m, Middleware::Retry(_))),
            has_dlq_middleware: self.output_has_middleware(|m| matches!(m, Middleware::Dlq(_))),
            transient_failure_policy,
            err_tx: err_tx.clone(),
            commit_semaphore: commit_semaphore.clone(),
            drops: drops.cloned(),
            sends: sends.cloned(),
        });
        // One batch is sent in its own task while the next is read. Sends never overlap, so
        // they stay in source order, and a started send is never cancelled.
        let mut sending: Option<JoinHandle<SendTaskOutput>> = None;
        let mut send_parts = Some((
            commit_tasks,
            BatchScratch::with_capacity(self.options.batch_size),
        ));
        let mut prefetched: Option<(Vec<crate::CanonicalMessage>, BatchCommitFunc)> = None;
        // Set once reading stops; returned when the last send has finished.
        let mut ending: Option<anyhow::Result<bool>> = None;
        let mut idle = false;
        // Messages processed since the last cooperative yield (see YIELD_EVERY_MSGS).
        let mut since_yield = 0usize;
        let mut reaped: Reaped = None;
        let mut run_result = loop {
            if let Some(joined) = reaped.take() {
                match joined {
                    Ok((result, tasks, scratch)) => {
                        send_parts = Some((tasks, scratch));
                        if let Err(err) = result {
                            if !matches!(ending, Some(Err(_))) {
                                ending = Some(Err(err));
                            }
                        }
                    }
                    Err(join_error) => {
                        // Its batch is never committed; ordered commits behind it must not wait.
                        commit_router.abandon();
                        ending = Some(Err(anyhow::anyhow!(
                            "The send task failed; its batch was never committed: {join_error}"
                        )));
                    }
                }
                // Cooperatively yield, amortized over messages processed so a
                // hot loop can't starve other tasks (see YIELD_EVERY_MSGS).
                if since_yield >= YIELD_EVERY_MSGS {
                    since_yield = 0;
                    tokio::task::yield_now().await;
                }
            }
            if sending.is_none() {
                if let Some((messages, commit)) = prefetched.take() {
                    if matches!(ending, Some(Err(_))) {
                        // As in the concurrent runner: a batch read but not sent goes back.
                        let _ = commit(vec![MessageDisposition::Nack; messages.len()]).await;
                    } else if let Some((tasks, scratch)) = send_parts.take() {
                        since_yield += messages.len();
                        sending = Some(spawn_send(&send_context, messages, commit, tasks, scratch));
                    }
                }
                if sending.is_none() {
                    if let Some(result) = ending.take() {
                        break result;
                    }
                }
            }
            let want_receive = prefetched.is_none() && !idle && ending.is_none();
            select! {
                Ok(err) = err_rx.recv(), if ending.is_none() => ending = Some(Err(err)),

                _ = shutdown_rx.recv(), if ending.is_none() => {
                    info!("Shutdown signal received in sequential runner for route '{}'.", route_display_name(name));
                    ending = Some(Ok(true)); // Stopped by shutdown signal
                }

                _ = pause_after_empty_batch(self.options.empty_batch_delay_ms), if idle && ending.is_none() => {
                    idle = false;
                }

                step = next_step(
                    want_receive.then(|| receive_with_permit(
                        consumer.as_mut(),
                        self.options.batch_size,
                        read_semaphore.as_ref(),
                        &mut read_permit,
                        flush_gate.as_ref(),
                    )),
                    &mut sending,
                    &mut reaped,
                ), if want_receive || sending.is_some() => {
                    match step {
                        Some(Ok(batch)) if batch.messages.is_empty() => {
                            if self.options.exit_on_empty {
                                info!("Consumer for route '{}' drained (empty batch, exit_on_empty). Shutting down.", route_display_name(name));
                                ending = Some(Ok(false)); // Graceful drain-then-exit
                            } else {
                                if let Some(gate) = &flush_gate {
                                    gate.nudge();
                                }
                                idle = true;
                            }
                        }
                        Some(Ok(batch)) => {
                            debug!("Received a batch of {} messages sequentially", batch.messages.len());
                            let mut commit = commit_router.wrap(batch.commit, seq_counter, read_permit.take());
                            seq_counter += 1;
                            if let Some(gate) = &flush_gate {
                                commit = gate.hold(commit);
                            }
                            prefetched = Some((batch.messages, commit));
                        }
                        Some(Err(ConsumerError::EndOfStream)) => {
                            info!("Consumer for route '{}' reached end of stream. Shutting down.", route_display_name(name));
                            ending = Some(Ok(false)); // Graceful exit
                        }
                        Some(Err(ConsumerError::Connection(e))) => {
                            // Propagate error to trigger reconnect by the outer loop
                            ending = Some(Err(e));
                        }
                        Some(Err(ConsumerError::Gap { requested, base })) => {
                            // Propagate gap error to trigger reconnect by the outer loop
                            ending = Some(Err(anyhow::anyhow!("Consumer gap: requested offset {requested} but earliest available is {base}")));
                        }
                        Some(Err(ConsumerError::Permanent(e))) => {
                            // Non-retryable: shut the route down instead of reconnecting
                            // and re-reading the same poison message forever.
                            ending = Some(Err(ConsumerError::Permanent(e).into()));
                        }
                        // Only a send finished; it waits in `reaped`.
                        None => {}
                    }
                }
            }
        };
        // A failed send task took its commit tasks with it.
        let (mut commit_tasks, _) =
            send_parts.unwrap_or_else(|| (JoinSet::new(), BatchScratch::with_capacity(0)));

        // A clean end flushes and commits what is held; a failed one commits nothing more.
        if let Some(gate) = &flush_gate {
            if run_result.is_ok() {
                gate.close();
            } else {
                gate.abandon();
            }
        }
        // Drain errors while waiting for tasks to finish to prevent deadlocks and lost errors
        loop {
            select! {
                biased;
                res = err_rx.recv() => {
                    if let Ok(err) = res {
                        error!("Error reported during shutdown: {}", err);
                        if matches!(&run_result, Ok(false)) {
                            run_result = Err(err);
                        }
                    }
                }
                res = commit_tasks.join_next() => {
                    match res {
                        Some(res) => report_join_result(res, "Commit task"),
                        None => break,
                    }
                }
            }
        }
        drop(err_rx);
        if let Some(gate) = flush_gate {
            gate.shutdown().await;
        }
        commit_router.shutdown().await;
        run_consumer_disconnect_hook(name, consumer.as_ref()).await;
        run_publisher_disconnect_hook(name, &publisher, disconnect_outcome(&run_result)).await;
        run_result
    }

    /// The main concurrent runner for when concurrency > 1.
    pub(super) async fn run_concurrently(
        &self,
        name: &str,
        shutdown_rx: async_channel::Receiver<()>,
        ready_tx: Option<Sender<()>>,
        drops: Option<&Arc<RwLock<DropReport>>>,
        sends: Option<&Arc<PendingSends>>,
        no_resume: bool,
    ) -> anyhow::Result<bool> {
        let source_has_position = self.source_has_position();
        let relaxed_output;
        let output =
            match relax_object_naming(name, source_has_position, &self.input, &self.output)? {
                Some((endpoint, reason)) => {
                    warn!(route = name, "{reason}");
                    relaxed_output = endpoint;
                    &relaxed_output
                }
                None => &self.output,
            };
        let synced_output;
        let output = match sync_file_sinks_behind_saved_position(
            name,
            &self.input,
            output,
            source_has_position,
            no_resume,
        ) {
            Some(endpoint) => {
                synced_output = endpoint;
                &synced_output
            }
            None => output,
        };
        let unsynced_output;
        let output = match relax_object_store_fsync(
            name,
            &self.input,
            output,
            source_has_position,
            no_resume,
        ) {
            Some(endpoint) => {
                unsynced_output = endpoint;
                &unsynced_output
            }
            None => output,
        };
        let source_metadata_required =
            output_requires_source_metadata(name, output, source_has_position)?;
        check_source_position_available(name, &self.input, source_metadata_required)?;
        let source_metadata_required = source_metadata_required
            || (source_has_position && output_keys_on_source_position(name, output)?);
        // A write-time name is minted inside the worker pool, so it is arrival order here
        // whether or not the input could have supplied a position instead. Checked after the
        // relaxation above, so a route it just moved onto write-time names is warned about too.
        if output_has_write_time_named_object_store(name, output, source_has_position)? {
            warn!(
                route = name,
                concurrency = self.options.concurrency,
                "object_store sink names objects by write time, which this route's worker pool \
                 makes arrival order rather than source order. Replaying a change stream through \
                 the bucket can reorder updates to the same key. Set name_by: source_position \
                 where the input carries a replay position; otherwise concurrency: 1 is the only \
                 remedy."
            );
        }
        let publisher =
            create_publisher_from_route_with_source_position(name, output, source_has_position)
                .await?;
        let transient_failure_policy = transient_publish_failure_policy(
            &self.input,
            output_passes_through_http_status(name, output)?,
        );
        let mut consumer = create_consumer_from_route_with_policy(
            name,
            &self.input,
            source_metadata_required,
            no_resume,
        )
        .await?;
        consumer.set_exit_on_empty(self.options.exit_on_empty);
        if let Err(err) = run_publisher_connect_hook(name, &publisher).await {
            run_publisher_disconnect_hook(name, &publisher, DisconnectOutcome::Failed).await;
            return Err(err);
        }
        if let Err(err) = run_consumer_connect_hook(name, consumer.as_ref()).await {
            run_consumer_disconnect_hook(name, consumer.as_ref()).await;
            run_publisher_disconnect_hook(name, &publisher, DisconnectOutcome::Failed).await;
            return Err(err);
        }
        if let Some(tx) = ready_tx {
            let _ = tx.send(()).await;
        }
        let (err_tx, err_rx) = bounded(1); // For critical, route-stopping errors

        // --- Publish Dispatch ---
        // Workers send concurrently, so batches can reach the sink out of source order. An
        // ordered sink (`requires_ordered_publish()`) gets a per-batch [`OrderTicket`] that
        // sequences the whole output chain; a `parallel` marker on the output takes over that
        // wait, so the entries above it overlap.
        let ordered_publish = publisher.requires_ordered_publish();
        if ordered_publish {
            debug!(
                "Route '{}' publishes to an order-sensitive sink: sends are sequenced.",
                name
            );
        } else if self.output_has_middleware(|m| matches!(m, Middleware::Parallel(_))) {
            warn!(
                "Route '{name}' has `parallel` on its output, but its sink accepts batches in any \
                 order, so the marker has no effect; `concurrency` already overlaps the sends."
            );
        }
        // channel capacity is measured in batches, not messages
        let work_capacity = self.options.concurrency;
        let (work_tx, work_rx) = bounded::<(
            Vec<crate::CanonicalMessage>,
            BatchCommitFunc,
            Option<OrderTicket>,
        )>(work_capacity);
        // --- Commit Dispatch ---
        // Cumulative-ack brokers (Kafka/AMQP) must commit in order, so their commits
        // are funnelled through a single sequencer to prevent data loss. Individual-ack
        // brokers commit concurrently (bounded by commit_concurrency_limit) so the
        // per-batch ack round trip no longer caps throughput.
        let commit_router = CommitRouter::new(
            consumer.commit_requires_order(),
            self.options.commit_concurrency_limit,
            // One batch per worker, a full work queue, and the one being enqueued.
            self.options.concurrency + work_capacity + 1,
        );
        // Shared across workers so the limit bounds total commits in flight, not per-worker.
        let commit_semaphore = commit_router.dispatch_semaphore();
        let read_semaphore = commit_router.read_semaphore();
        let mut read_permit = None;
        let flush_gate = commit_router.flush_gate(&publisher, &err_tx);

        // --- Worker Pool ---
        let mut join_set = JoinSet::new();
        for i in 0..self.options.concurrency {
            let work_rx_clone = work_rx.clone();
            let publisher = Arc::clone(&publisher);
            let err_tx = err_tx.clone();
            let commit_semaphore = commit_semaphore.clone();
            let mut commit_tasks = JoinSet::new();
            let has_retry_middleware =
                self.output_has_middleware(|m| matches!(m, Middleware::Retry(_)));
            let has_dlq_middleware =
                self.output_has_middleware(|m| matches!(m, Middleware::Dlq(_)));
            let batch_size = self.options.batch_size;
            // Owned per worker: the borrow cannot outlive this loop iteration.
            let drops = drops.cloned();
            let sends = sends.cloned();
            join_set.spawn(async move {
                debug!("Starting worker {}", i);
                let mut batch_scratch = BatchScratch::with_capacity(batch_size);
                let mut since_yield = 0usize;
                // A worker only leaves this loop when the route is going down, so closing the
                // channel on the way out (panic included) unblocks a producer waiting on a full one.
                let work = CloseOnDrop(work_rx_clone);
                while let Ok((messages, commit_func, ticket)) = work.0.recv().await {
                    let batch_len = messages.len();
                    if let Err(err) = send_batch_and_commit(
                        &publisher,
                        messages,
                        commit_func,
                        has_retry_middleware,
                        has_dlq_middleware,
                        transient_failure_policy,
                        &err_tx,
                        commit_semaphore.as_ref(),
                        &mut commit_tasks,
                        &mut batch_scratch,
                        drops.as_ref(),
                        sends.as_deref().map(|sends| (sends, i)),
                        ticket,
                    )
                    .await
                    {
                        error!("Worker failed to process message batch: {}", err);
                        report_route_error(&err_tx, err, "Could not send error to main task");
                        break;
                    }
                    // Amortized cooperative yield (see YIELD_EVERY_MSGS).
                    since_yield += batch_len;
                    if since_yield >= YIELD_EVERY_MSGS {
                        since_yield = 0;
                        tokio::task::yield_now().await;
                    }
                }
                drop(work);
                // Wait for all in-flight commits to complete
                while let Some(res) = commit_tasks.join_next().await {
                    report_join_result(res, "Commit task");
                }
            });
        }

        let mut seq_counter = 0u64;
        // Tail of the publish-ordering chain: what the *next* batch waits on. `None`
        // while the sink tolerates unordered sends.
        let mut prev_publish: Option<tokio::sync::oneshot::Receiver<()>> = None;
        // Messages enqueued to workers since the last cooperative yield (see YIELD_EVERY_MSGS).
        let mut since_yield = 0usize;
        // Holds an error that caused the loop to break, to be returned after graceful shutdown.
        let mut loop_error: Option<anyhow::Error> = None;
        // Set when the loop breaks on a graceful terminal -- an empty batch under
        // exit_on_empty, or a source reporting end of stream -- so we report a
        // completion rather than a shutdown-driven exit.
        let mut drained = false;
        // The batch that could not be enqueued because the workers were already gone.
        let mut leftover = None;
        // A panicked worker never commits its batch, which leaves a gap in the commit sequence.
        let mut worker_panicked = false;
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
                            worker_panicked |= e.is_panic();
                            error!("A worker task panicked: {}. Shutting down route.", e);
                            loop_error = Some(e.into());
                        }
                    }
                    break;
                }

                _ = shutdown_rx.recv() => {
                    info!("Shutdown signal received in concurrent runner for route '{}'.", route_display_name(name));
                    break;
                }

                res = receive_with_permit(
                    consumer.as_mut(),
                    self.options.batch_size,
                    read_semaphore.as_ref(),
                    &mut read_permit,
                    flush_gate.as_ref(),
                ) => {
                    let (messages, commit) = match res {
                        Ok(batch) => {
                            if batch.messages.is_empty() {
                                if self.options.exit_on_empty {
                                    info!("Consumer for route '{}' drained (empty batch, exit_on_empty). Shutting down.", route_display_name(name));
                                    drained = true;
                                    break; // Graceful drain-then-exit
                                }
                                if let Some(gate) = &flush_gate {
                                    gate.nudge();
                                }
                                pause_after_empty_batch(self.options.empty_batch_delay_ms).await;
                                continue; // No messages, loop to select! again
                            }
                            (batch.messages, batch.commit)
                        }
                        Err(ConsumerError::EndOfStream) => {
                            info!("Consumer for route '{}' reached end of stream. Shutting down.", route_display_name(name));
                            // Without this the tail below reads "no shutdown was
                            // requested" as "reconnect", and the outer loop reruns
                            // the whole source again, forever.
                            drained = true;
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
                        Err(ConsumerError::Permanent(e)) => {
                            // Non-retryable: shut the route down instead of reconnecting
                            // and re-reading the same poison message forever.
                            loop_error = Some(ConsumerError::Permanent(e).into());
                            break;
                        }
                    };
                    debug!("Received a batch of {} messages concurrently", messages.len());

                    // Wrap the commit function to route it through the sequencer.
                    // Only advance the sequence counter after we've successfully enqueued
                    // the work item to avoid creating sequence gaps if the work channel
                    // is closed while producing batches.
                    let seq = seq_counter;
                    let batch_len = messages.len();
                    let mut wrapped_commit = commit_router.wrap(commit, seq, read_permit.take());
                    if let Some(gate) = &flush_gate {
                        wrapped_commit = gate.hold(wrapped_commit);
                    }
                    let ticket = ordered_publish.then(|| {
                        let (release, next_prev) = tokio::sync::oneshot::channel();
                        let ticket = OrderTicket {
                            prev: prev_publish.take(),
                            _release: release,
                        };
                        prev_publish = Some(next_prev);
                        ticket
                    });

                    match work_tx.send((messages, wrapped_commit, ticket)).await {
                        Ok(()) => {
                            seq_counter += 1;
                        }
                        Err(e) => {
                            warn!("Work channel closed, cannot process more messages concurrently. Shutting down.");
                            // Keep the batch: it is nacked below, after the batches queued
                            // ahead of it, so the commit sequence stays in order.
                            leftover = Some(e.into_inner());
                            break;
                        }
                    }

                    // Amortized cooperative yield (see YIELD_EVERY_MSGS).
                    since_yield += batch_len;
                    if since_yield >= YIELD_EVERY_MSGS {
                        since_yield = 0;
                        tokio::task::yield_now().await;
                    }
                }
            }
        }

        // --- Graceful Shutdown ---
        // Close the work channel so workers drain their current messages and exit the loop.
        // This applies on both normal shutdown AND error paths, ensuring in-flight commits
        // are not aborted mid-sequence.
        drop(work_tx);
        // A clean end flushes and commits what is held; a failed one commits nothing more.
        if let Some(gate) = &flush_gate {
            if loop_error.is_some() || worker_panicked {
                gate.abandon();
            } else {
                gate.close();
            }
        }
        // A panicked worker never commits its batch. Ordered commits behind that gap would
        // wait on it forever, so they are failed at once and stay uncommitted for redelivery.
        if worker_panicked {
            commit_router.abandon();
        }
        // Wait for all worker tasks to complete.
        while let Some(res) = join_set.join_next().await {
            if matches!(&res, Err(e) if e.is_panic()) {
                worker_panicked = true;
                commit_router.abandon();
                if let Some(gate) = &flush_gate {
                    gate.abandon();
                }
            }
            report_join_result(res, "Worker task");
        }
        // Batches no worker picked up are nacked for redelivery. After a panic an ordered
        // nack fails on the abandoned sequencer, which leaves the batch uncommitted.
        let unprocessed = std::iter::from_fn(|| work_rx.try_recv().ok()).chain(leftover);
        for (messages, commit, _) in unprocessed {
            let _ = commit(vec![MessageDisposition::Nack; messages.len()]).await;
        }

        if let Some(gate) = flush_gate {
            gate.shutdown().await;
        }
        // Close sequencer (if any) now that all in-flight commits have drained.
        commit_router.shutdown().await;
        run_consumer_disconnect_hook(name, consumer.as_ref()).await;
        // Decided once so the outcome the publisher is told cannot drift from what the
        // route returns. A drain-then-exit is a graceful completion, not a shutdown-driven
        // exit; what is left after it is a shutdown signal (channel empty means it was
        // closed/consumed) or a dropped connection the outer loop should reconnect.
        let result = if let Some(err) = loop_error {
            Err(err)
        } else if worker_panicked {
            Err(anyhow::anyhow!(
                "A worker task panicked; its batch was never committed"
            ))
        } else if let Ok(err) = err_rx.try_recv() {
            Err(err)
        } else if drained {
            Ok(false)
        } else {
            Ok(shutdown_rx.is_empty())
        };
        run_publisher_disconnect_hook(name, &publisher, disconnect_outcome(&result)).await;
        result
    }
}
