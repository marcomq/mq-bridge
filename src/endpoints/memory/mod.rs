//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

mod endpoint;
pub mod memory_transport;
pub mod transport;

#[cfg(any(unix, windows))]
mod framed;
#[cfg(unix)]
pub mod ipc_unix;
#[cfg(windows)]
pub mod ipc_windows;

// Re-export the main endpoint types for backward compatibility
pub use endpoint::{
    get_or_create_channel, get_or_create_response_channel, MemoryChannel, MemoryConsumer,
    MemoryPublisher, MemoryQueueConsumer, MemoryResponseChannel, MemorySubscriber,
};
