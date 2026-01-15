//! Error types for ironflow.

use thiserror::Error;

/// A `Result` alias with [`enum@Error`] as the error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur in ironflow operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Failed to serialize or deserialize event/effect data.
    ///
    /// This typically indicates a mismatch between the stored event format
    /// and the current `Workflow::Event` type definition.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Failed to deserialize an event during replay.
    ///
    /// Includes context about which event failed: workflow type, workflow ID,
    /// and the event's sequence number (0-indexed position in the stream).
    #[error(
        "failed to deserialize event at sequence {sequence} for {workflow_type}:{workflow_id}: {source}"
    )]
    EventDeserialization {
        /// The workflow type identifier.
        workflow_type: &'static str,
        /// The workflow instance ID.
        workflow_id: String,
        /// The event's position in the stream (0-indexed).
        sequence: usize,
        /// The underlying deserialization error.
        #[source]
        source: serde_json::Error,
    },

    /// PostgreSQL storage error.
    ///
    /// Preserves the full `sqlx::Error` for matching on specific database
    /// error conditions (connection timeout, constraint violation, etc.).
    #[cfg(feature = "postgres")]
    #[error("postgres error: {0}")]
    Postgres(#[from] sqlx::Error),

    /// Workflow type was not registered in the service registry.
    #[error("unknown workflow type: {0}")]
    UnknownWorkflowType(String),

    /// Workflow type was registered more than once.
    #[error("duplicate workflow type registration: {0}")]
    DuplicateWorkflowType(String),
}

impl Error {
    /// Create an event deserialization error with context.
    pub fn event_deserialization(
        workflow_type: &'static str,
        workflow_id: impl Into<String>,
        sequence: usize,
        source: serde_json::Error,
    ) -> Self {
        Error::EventDeserialization {
            workflow_type,
            workflow_id: workflow_id.into(),
            sequence,
            source,
        }
    }
}
