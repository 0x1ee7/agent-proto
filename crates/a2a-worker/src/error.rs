//! Worker runtime errors.

use a2a_core::envelope::ReplyError;
use a2a_core::task::{StoreError, TaskError};
use bus_api::error::BusError;

/// Failure of a worker operation that is not a per-request wire answer.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The bus refused or failed an operation the worker must complete.
    #[error("bus failure: {0}")]
    Bus(#[from] BusError),
    /// Answering an inbound request failed.
    #[error("reply failure: {0}")]
    Reply(#[from] ReplyError),
    /// The durable task store failed; the worker must stop serving.
    #[error("task store failure: {0}")]
    Store(StoreError),
    /// The task engine rejected an internal transition.
    #[error("task engine: {0}")]
    Tasks(TaskError),
    /// The injected clock has no time source.
    #[error("clock unavailable")]
    Clock,
    /// The configuration is not usable.
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    /// The request requires a capability the worker does not provide.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// A value the worker produced or received cannot be represented.
    #[error("invalid value: {0}")]
    Invalid(&'static str),
}

impl From<TaskError> for WorkerError {
    fn from(err: TaskError) -> Self {
        match err {
            TaskError::Store(inner) => Self::Store(inner),
            other => Self::Tasks(other),
        }
    }
}
