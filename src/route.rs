//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use tokio::task;

use crate::endpoints::{create_consumer_from_route, create_publisher_from_route};
pub use crate::models::Route;

impl Route {
    pub async fn run(&self, name: &str) -> anyhow::Result<()> {
        let publisher = create_publisher_from_route(name, &self.output.endpoint_type).await?;
        let mut consumer = create_consumer_from_route(name, &self.input.endpoint_type).await?;
        loop {
            let (message, commit) = consumer.receive().await?;
            let response = publisher.send(message).await?;
            task::spawn(commit(response));
        }
    }
}