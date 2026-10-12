use crate::traits::{Handled, HandlerError};
use crate::CanonicalMessage;

/// Internal trait to convert success types into the [Handled] enum.
pub trait IntoHandled {
    fn into_handled(self) -> Handled;
}

impl IntoHandled for Handled {
    fn into_handled(self) -> Handled {
        self
    }
}

impl IntoHandled for () {
    fn into_handled(self) -> Handled {
        Handled::Ack
    }
}

impl IntoHandled for CanonicalMessage {
    fn into_handled(self) -> Handled {
        Handled::Publish(self)
    }
}

impl IntoHandled for Option<CanonicalMessage> {
    fn into_handled(self) -> Handled {
        match self {
            Some(msg) => Handled::Publish(msg),
            None => Handled::Ack,
        }
    }
}

/// A trait for types that can be converted into a `Result<Handled, HandlerError>`.
///
/// This allows handlers to return ergonomic types instead of the explicit `Handled` enum.
///
/// ### Supported Success Types:
/// - `()`: Equivalent to `Handled::Ack` (Processed successfully).
/// - `CanonicalMessage`: Equivalent to `Handled::Publish(msg)` (Send a response).
/// - `Option<CanonicalMessage>`: `Some` sends a response, `None` acknowledges (Ack).
/// - `Handled`: The original enum (for full control).
///
/// ### Supported Error Types (via `ToHandlerError`):
/// - `anyhow::Error`: Treated as `Retryable`, unless it wraps a `HandlerError` or an
///   `InvalidConfig`, whose classification is kept.
/// - `String` / `&str`: Treated as `NonRetryable`.
/// - `HandlerError`: The internal error type for explicit control.
///
/// # Examples
///
/// ```rust
/// use mq_bridge::{CanonicalMessage, Handled};
///
/// struct MyData { id: u32 }
///
/// async fn handle_reply(_: MyData) -> CanonicalMessage {
///     CanonicalMessage::from("Response")
/// }
///
/// // 2. Use anyhow::Result (Ok(()) is now inferred correctly in real logic)
/// async fn handle_fail(_: MyData) -> anyhow::Result<()> {
///     // Some logic that might use the '?' operator
///     if true { anyhow::bail!("Something went wrong"); }
///     
///     Ok(()) // No Handled::Ack needed
/// }
/// ```
pub trait IntoHandlerResult: Send + Sync + 'static {
    fn into_handler_result(self) -> Result<Handled, HandlerError>;
}

/// A marker for types that are considered 'ergonomic' (not the standard Result<Handled, HandlerError>).
/// This prevents type inference ambiguity in existing code.
pub trait ErgonomicResponse: IntoHandlerResult {}

impl IntoHandlerResult for Handled {
    fn into_handler_result(self) -> Result<Handled, HandlerError> {
        Ok(self)
    }
}
impl ErgonomicResponse for Handled {}

impl IntoHandlerResult for () {
    fn into_handler_result(self) -> Result<Handled, HandlerError> {
        Ok(Handled::Ack)
    }
}
impl ErgonomicResponse for () {}

impl IntoHandlerResult for CanonicalMessage {
    fn into_handler_result(self) -> Result<Handled, HandlerError> {
        Ok(Handled::Publish(self))
    }
}
impl ErgonomicResponse for CanonicalMessage {}

impl IntoHandlerResult for Option<CanonicalMessage> {
    fn into_handler_result(self) -> Result<Handled, HandlerError> {
        Ok(self.into_handled())
    }
}
impl ErgonomicResponse for Option<CanonicalMessage> {}

/// Explicit implementation for the standard result type.
/// Note: This is NOT an ErgonomicResponse to maintain disjoint implementations in IntoTypedHandler.
impl IntoHandlerResult for Result<Handled, HandlerError> {
    fn into_handler_result(self) -> Result<Handled, HandlerError> {
        self
    }
}

impl<E> IntoHandlerResult for Result<(), E>
where
    E: ToHandlerError + Send + Sync + 'static,
{
    fn into_handler_result(self) -> Result<Handled, HandlerError> {
        match self {
            Ok(_) => Ok(Handled::Ack),
            Err(err) => Err(err.to_handler_error()),
        }
    }
}
impl<E> ErgonomicResponse for Result<(), E> where E: ToHandlerError + Send + Sync + 'static {}

impl<E> IntoHandlerResult for Result<CanonicalMessage, E>
where
    E: ToHandlerError + Send + Sync + 'static,
{
    fn into_handler_result(self) -> Result<Handled, HandlerError> {
        match self {
            Ok(msg) => Ok(Handled::Publish(msg)),
            Err(err) => Err(err.to_handler_error()),
        }
    }
}
impl<E> ErgonomicResponse for Result<CanonicalMessage, E> where
    E: ToHandlerError + Send + Sync + 'static
{
}

impl<E> IntoHandlerResult for Result<Option<CanonicalMessage>, E>
where
    E: ToHandlerError + Send + Sync + 'static,
{
    fn into_handler_result(self) -> Result<Handled, HandlerError> {
        match self {
            Ok(val) => Ok(val.into_handled()),
            Err(err) => Err(err.to_handler_error()),
        }
    }
}
impl<E> ErgonomicResponse for Result<Option<CanonicalMessage>, E> where
    E: ToHandlerError + Send + Sync + 'static
{
}

/// Internal trait to convert error types into [HandlerError].
pub trait ToHandlerError {
    fn to_handler_error(self) -> HandlerError;
}

impl ToHandlerError for HandlerError {
    fn to_handler_error(self) -> HandlerError {
        self
    }
}

impl ToHandlerError for anyhow::Error {
    fn to_handler_error(self) -> HandlerError {
        self.into()
    }
}

impl ToHandlerError for String {
    fn to_handler_error(self) -> HandlerError {
        crate::errors::ProcessingError::NonRetryable(anyhow::anyhow!(self))
    }
}

impl ToHandlerError for &str {
    fn to_handler_error(self) -> HandlerError {
        crate::errors::ProcessingError::NonRetryable(anyhow::anyhow!(self.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ProcessingError;

    #[test]
    fn test_into_handled_converts_supported_success_types() {
        assert!(matches!(().into_handled(), Handled::Ack));
        assert!(matches!(
            None::<CanonicalMessage>.into_handled(),
            Handled::Ack
        ));

        let message = CanonicalMessage::from("payload");
        match message.clone().into_handled() {
            Handled::Publish(published) => assert_eq!(published.get_payload_str(), "payload"),
            Handled::Ack => panic!("expected publish result"),
        }

        match Some(message).into_handled() {
            Handled::Publish(published) => assert_eq!(published.get_payload_str(), "payload"),
            Handled::Ack => panic!("expected publish result"),
        }
    }

    #[test]
    fn test_into_handler_result_converts_ok_variants() {
        assert!(matches!(().into_handler_result().unwrap(), Handled::Ack));
        assert!(matches!(
            Option::<CanonicalMessage>::None
                .into_handler_result()
                .unwrap(),
            Handled::Ack
        ));

        match CanonicalMessage::from("reply")
            .into_handler_result()
            .unwrap()
        {
            Handled::Publish(message) => assert_eq!(message.get_payload_str(), "reply"),
            Handled::Ack => panic!("expected publish result"),
        }

        match Result::<Option<CanonicalMessage>, &str>::Ok(Some(CanonicalMessage::from("maybe")))
            .into_handler_result()
            .unwrap()
        {
            Handled::Publish(message) => assert_eq!(message.get_payload_str(), "maybe"),
            Handled::Ack => panic!("expected publish result"),
        }
    }

    #[test]
    fn test_into_handler_result_and_to_handler_error_convert_errors() {
        let retryable =
            Result::<CanonicalMessage, anyhow::Error>::Err(anyhow::anyhow!("temporary"))
                .into_handler_result()
                .unwrap_err();
        assert!(matches!(retryable, ProcessingError::Retryable(_)));

        let non_retryable = Result::<(), &str>::Err("permanent")
            .into_handler_result()
            .unwrap_err();
        assert!(matches!(non_retryable, ProcessingError::NonRetryable(_)));

        let classified = anyhow::Error::new(ProcessingError::NonRetryable(anyhow::anyhow!("bad")));
        let kept = Result::<(), anyhow::Error>::Err(classified)
            .into_handler_result()
            .unwrap_err();
        assert!(matches!(kept, ProcessingError::NonRetryable(_)));

        let string_error = "string failure".to_string().to_handler_error();
        assert!(matches!(string_error, ProcessingError::NonRetryable(_)));

        let str_error = "str failure".to_handler_error();
        assert!(matches!(str_error, ProcessingError::NonRetryable(_)));
    }
}
