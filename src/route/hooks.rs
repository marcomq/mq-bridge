//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Connect and disconnect hooks run on the route's publisher and consumer.

use super::*;

pub(crate) async fn run_publisher_connect_hook(
    route_name: &str,
    publisher: &Arc<dyn MessagePublisher>,
) -> anyhow::Result<()> {
    if let Some(hook) = publisher.on_connect_hook() {
        hook.await.map_err(|err| {
            anyhow::anyhow!(
                "Publisher on_connect hook failed for route '{}': {}",
                route_name,
                err
            )
        })?;
    }
    Ok(())
}

pub(crate) async fn run_consumer_connect_hook(
    route_name: &str,
    consumer: &dyn MessageConsumer,
) -> anyhow::Result<()> {
    if let Some(hook) = consumer.on_connect_hook() {
        hook.await.map_err(|err| {
            anyhow::anyhow!(
                "Consumer on_connect hook failed for route '{}': {}",
                route_name,
                err
            )
        })?;
    }
    Ok(())
}

/// The outcome a runner's return value describes: `Ok(false)` is a natural end,
/// `Ok(true)` a shutdown, `Err` a failure.
pub(crate) fn disconnect_outcome(result: &anyhow::Result<bool>) -> DisconnectOutcome {
    match result {
        Ok(false) => DisconnectOutcome::Completed,
        Ok(true) => DisconnectOutcome::Stopped,
        Err(_) => DisconnectOutcome::Failed,
    }
}

pub(crate) async fn run_publisher_disconnect_hook(
    route_name: &str,
    publisher: &Arc<dyn MessagePublisher>,
    outcome: DisconnectOutcome,
) {
    let _ = publisher_disconnect_result(route_name, publisher, outcome).await;
}

/// Runs the publisher's disconnect hook and also returns its error, which is otherwise only logged.
pub(crate) async fn publisher_disconnect_result(
    route_name: &str,
    publisher: &Arc<dyn MessagePublisher>,
    outcome: DisconnectOutcome,
) -> anyhow::Result<()> {
    // Scoped rather than passed as an argument, so it reaches the sink through any depth
    // of middleware without each wrapper having to forward it.
    with_disconnect_outcome(outcome, async {
        let Some(hook) = publisher.on_disconnect_hook() else {
            return Ok(());
        };
        hook.await.inspect_err(|err| {
            warn!(
                "Publisher on_disconnect hook failed for route '{}': {}",
                route_name, err
            );
        })
    })
    .await
}

pub(crate) async fn run_consumer_disconnect_hook(route_name: &str, consumer: &dyn MessageConsumer) {
    if let Some(hook) = consumer.on_disconnect_hook() {
        if let Err(err) = hook.await {
            warn!(
                "Consumer on_disconnect hook failed for route '{}': {}",
                route_name, err
            );
        }
    }
}
