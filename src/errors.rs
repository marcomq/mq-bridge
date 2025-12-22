use thiserror::Error;

/// Errors that can occur during message processing (handling or publishing).
#[derive(Error, Debug)]
pub enum ProcessingError {
    /// A transient error occurred. The operation should be retried.
    #[error("retryable error: {0}")]
    Retryable(#[source] anyhow::Error),
    /// A permanent error occurred. The operation should not be retried.
    #[error("non-retryable error: {0}")]
    NonRetryable(#[source] anyhow::Error),
}

pub type HandlerError = ProcessingError;
pub type PublisherError = ProcessingError;

/// Errors that can occur when consuming messages.
#[derive(Error, Debug)]
pub enum ConsumerError {
    /// A transport-level or other error occurred that should trigger a reconnect.
    #[error("consumer connection error: {0}")]
    Connection(#[source] anyhow::Error),

    /// The consumer has reached the end of the stream and has shut down gracefully.
    #[error("consumer reached end of stream")]
    EndOfStream,
}

impl From<anyhow::Error> for ConsumerError {
    fn from(err: anyhow::Error) -> Self {
        // By default, we'll treat any generic error as a connection-level, retryable error.
        ConsumerError::Connection(err)
    }
}

impl From<anyhow::Error> for ProcessingError {
    fn from(err: anyhow::Error) -> Self {
        // Default to Retryable for generic errors. Callers should use
        // ProcessingError::NonRetryable directly for known permanent failures.
        ProcessingError::Retryable(err)
    }
}
