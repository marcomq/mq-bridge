use thiserror::Error;

/// Render an `anyhow` cause chain like `{:#}` does, but skip a link whose text an
/// earlier one already contains. Some client errors display their own source verbatim
/// (`sqlx::Error::Database` is `"error returned from database: {source}"`), so the plain
/// chain form printed every database error twice, joined by `: `.
fn chain(err: &anyhow::Error) -> String {
    let mut out = String::new();
    for cause in err.chain() {
        let text = cause.to_string();
        if out.is_empty() {
            out = text;
        } else if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
    }
    out
}

/// Errors that can occur during message processing (handling or publishing).
///
/// Every variant formats its whole cause chain. A plain `{}` prints
/// only the outermost `.context(...)`, and these errors are routinely flattened into a
/// string on the way to the route and the logs, which threw away whatever the client
/// library actually said (a driver reason code, an HTTP status detail).
#[derive(Error, Debug)]
pub enum ProcessingError {
    /// A transient error occurred. The operation should be retried.
    #[error("retryable error: {}", chain(.0))]
    Retryable(#[source] anyhow::Error),
    /// A permanent error occurred. The operation should not be retried.
    #[error("non-retryable error: {}", chain(.0))]
    NonRetryable(#[source] anyhow::Error),
    /// A connection-level error occurred. Used to signal broker disconnects, etc.
    #[error("connection error: {}", chain(.0))]
    Connection(#[source] anyhow::Error),
}
impl ProcessingError {
    pub fn is_connection_error(&self) -> bool {
        matches!(self, ProcessingError::Connection(_))
    }
}

pub type HandlerError = ProcessingError;
pub type PublisherError = ProcessingError;

/// Errors that can occur when consuming messages. Causes are formatted like
/// [`ProcessingError`]'s, and for the same reason.
#[derive(Error, Debug)]
pub enum ConsumerError {
    /// A transport-level or other error occurred that should trigger a reconnect.
    #[error("consumer connection error: {}", chain(.0))]
    Connection(#[source] anyhow::Error),

    /// A consumer gap was detected: the requested events were already garbage-collected.
    #[error("consumer gap: requested offset {requested} but earliest available is {base}")]
    Gap { requested: u64, base: u64 },

    /// The consumer has reached the end of the stream and has shut down gracefully.
    #[error("consumer reached end of stream")]
    EndOfStream,

    /// A permanent, non-retryable error: the message cannot be processed and must
    /// not be re-read (e.g. failed AEAD decryption/authentication of a payload).
    #[error("permanent consumer error: {}", chain(.0))]
    Permanent(#[source] anyhow::Error),
}

/// An endpoint configuration that cannot work as given. Return it from
/// `create_consumer` or `create_publisher`, and the route stops instead of
/// reconnecting: `config::resolve(value).map_err(InvalidConfig)?`.
#[derive(Error, Debug)]
#[error(transparent)]
pub struct InvalidConfig(pub anyhow::Error);

impl From<anyhow::Error> for ConsumerError {
    fn from(err: anyhow::Error) -> Self {
        // Preserve a classification the source already made — downgrading an existing
        // `Permanent` to `Connection` would make the route retry something that cannot heal.
        if err.is::<InvalidConfig>() {
            return ConsumerError::Permanent(err);
        }
        match err.downcast::<ConsumerError>() {
            Ok(classified) => classified,
            // Otherwise a generic error is connection-level, and therefore retryable.
            Err(err) => ConsumerError::Connection(err),
        }
    }
}

impl From<anyhow::Error> for ProcessingError {
    fn from(err: anyhow::Error) -> Self {
        if err.is::<InvalidConfig>() {
            return ProcessingError::NonRetryable(err);
        }
        // Default to Retryable for generic errors. Callers should use
        // ProcessingError::NonRetryable directly for known permanent failures.
        ProcessingError::Retryable(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_connection_error_only_matches_connection_variant() {
        assert!(ProcessingError::Connection(anyhow::anyhow!("offline")).is_connection_error());
        assert!(!ProcessingError::Retryable(anyhow::anyhow!("retry")).is_connection_error());
        assert!(!ProcessingError::NonRetryable(anyhow::anyhow!("stop")).is_connection_error());
    }

    #[test]
    fn test_anyhow_error_conversions_use_default_variants() {
        let consumer_error = ConsumerError::from(anyhow::anyhow!("consumer failure"));
        assert!(matches!(consumer_error, ConsumerError::Connection(_)));

        let processing_error = ProcessingError::from(anyhow::anyhow!("processing failure"));
        assert!(matches!(processing_error, ProcessingError::Retryable(_)));
    }

    #[test]
    fn test_anyhow_conversion_preserves_existing_classification() {
        let permanent = ConsumerError::from(anyhow::Error::new(ConsumerError::Permanent(
            anyhow::anyhow!("decrypt failed"),
        )));
        assert!(matches!(permanent, ConsumerError::Permanent(_)));

        let end = ConsumerError::from(anyhow::Error::new(ConsumerError::EndOfStream));
        assert!(matches!(end, ConsumerError::EndOfStream));
    }

    #[test]
    fn test_display_keeps_the_whole_cause_chain() {
        // What an endpoint does: a client-library error, then a `.context()` on top. The
        // context alone ("MQ connect failed") is useless without the reason code under it.
        let cause = anyhow::anyhow!("MQRC_NOT_AUTHORIZED (2035)").context("MQ connect failed");
        let rendered = ProcessingError::Retryable(cause).to_string();
        assert!(
            rendered.contains("MQ connect failed") && rendered.contains("MQRC_NOT_AUTHORIZED"),
            "cause chain was flattened away: {rendered}"
        );

        let cause = anyhow::anyhow!("broken pipe").context("read failed");
        let rendered = ConsumerError::Connection(cause).to_string();
        assert!(rendered.contains("broken pipe"), "{rendered}");
    }

    /// `sqlx::Error::Database` displays its own source verbatim, so the plain chain form
    /// rendered every database error twice, joined by `: `.
    #[test]
    fn test_display_does_not_repeat_a_self_describing_cause() {
        #[derive(Debug, Error)]
        #[error("column \"blob\" is of type bytea but expression is of type text at line 586")]
        struct Inner;

        #[derive(Debug, Error)]
        #[error("error returned from database: {0}")]
        struct Outer(#[source] Inner);

        let rendered = ProcessingError::NonRetryable(anyhow::Error::new(Outer(Inner))).to_string();
        assert_eq!(
            rendered,
            "non-retryable error: error returned from database: column \"blob\" is of type bytea \
             but expression is of type text at line 586"
        );
    }

    #[test]
    fn an_invalid_config_is_permanent_on_either_side_even_under_context() {
        let invalid =
            || anyhow::Error::new(InvalidConfig(anyhow::anyhow!("no url"))).context("route x");
        assert!(matches!(
            ConsumerError::from(invalid()),
            ConsumerError::Permanent(_)
        ));
        assert!(matches!(
            ProcessingError::from(invalid()),
            ProcessingError::NonRetryable(_)
        ));
        assert_eq!(format!("{:#}", invalid()), "route x: no url");
    }

    #[test]
    fn test_consumer_error_display_messages() {
        assert_eq!(
            ConsumerError::Gap {
                requested: 42,
                base: 9
            }
            .to_string(),
            "consumer gap: requested offset 42 but earliest available is 9"
        );
        assert_eq!(
            ConsumerError::EndOfStream.to_string(),
            "consumer reached end of stream"
        );
    }
}
