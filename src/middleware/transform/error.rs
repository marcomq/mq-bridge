//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::traits::PublisherError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ErrorKind {
    Parse,
    #[cfg(feature = "zen")]
    Expression,
    Coercion,
    Content,
    MissingRequired,
    TypeMismatch,
    Enum,
}

impl ErrorKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Parse => "parse",
            #[cfg(feature = "zen")]
            ErrorKind::Expression => "expression",
            ErrorKind::Coercion => "coercion",
            ErrorKind::Content => "content",
            ErrorKind::MissingRequired => "missing_required",
            ErrorKind::TypeMismatch => "type_mismatch",
            ErrorKind::Enum => "enum",
        }
    }
}

/// A transformation failure, always permanent: the same bytes will fail the same way, so
/// these surface as `NonRetryable` and are meant to be routed to a DLQ.
#[derive(Debug)]
pub(super) struct TransformError {
    pub(super) path: String,
    pub(super) kind: ErrorKind,
    pub(super) detail: String,
}

impl TransformError {
    pub(super) fn new(path: String, kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            path,
            kind,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for TransformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "transform failed at {} [{}]: {}",
            self.path,
            self.kind.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for TransformError {}

impl From<TransformError> for PublisherError {
    fn from(err: TransformError) -> Self {
        PublisherError::NonRetryable(anyhow::Error::new(err))
    }
}
