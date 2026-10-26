//! Test and simulation control surface (RFC-0011 [0011-060..064]).
//!
//! [`BusControl`] lives in `bus-api` so that conformance can drive any binding
//! through one trait. Only simulation bindings need to implement it fully; a
//! binding without a virtual clock or fault switch reports
//! [`BusError::Unsupported`] naming the operation.

use std::num::NonZeroUsize;
use std::time::Duration;

use a2a_types::ids::TenantId;

use crate::error::BusError;
use crate::name::{ConsumerName, Principal, StreamName};
use crate::value::Digest;

/// A probability in the closed interval [0, 1].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Probability(f64);

impl Probability {
    /// Returns `None` when `value` is NaN or outside [0, 1].
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        if (0.0..=1.0).contains(&value) {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The probability value.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Distribution of an injected delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayDist {
    /// Always this long.
    Fixed(Duration),
    /// Uniform in `[min, max]`; `min` must not exceed `max`.
    Uniform {
        /// Lower bound.
        min: Duration,
        /// Upper bound.
        max: Duration,
    },
}

/// A fault switch. Faults never change stored content (RFC-0011 [0011-062]).
#[derive(Debug, Clone, PartialEq)]
pub enum Fault {
    /// Drop requests, replies, and consumer deliveries with this probability.
    Drop(Probability),
    /// Duplicate requests and deliveries with this probability.
    Duplicate(Probability),
    /// Reorder deliveries among the first `window` eligible candidates. Never
    /// breaks per-subject order.
    Reorder {
        /// Number of candidates considered.
        window: NonZeroUsize,
    },
    /// Delay visibility of stored messages.
    Delay(DelayDist),
    /// Split principals into two sets that cannot reach each other.
    Partition {
        /// First side.
        set_a: Vec<Principal>,
        /// Second side.
        set_b: Vec<Principal>,
    },
}

/// Simulation control of a binding. Time is virtual where the binding has
/// one, so tests never sleep on the wall clock.
///
/// Every method except [`BusControl::seed`] returns
/// [`BusError::Unsupported`] naming the fault or clock operation when the
/// binding cannot provide it. The conformance suite reports those rows as
/// unexercised rather than failed.
pub trait BusControl: Send + Sync {
    /// The seed that determines every fault decision.
    fn seed(&self) -> u64;

    /// Current time since the binding was created.
    ///
    /// # Errors
    /// [`BusError::Unsupported`] when the binding has no clock it can report.
    fn now(&self) -> Result<Duration, BusError>;

    /// Advances virtual time by `by`, waking timers that became due.
    ///
    /// # Errors
    /// [`BusError::Unsupported`] when the binding has no virtual clock.
    fn advance(&self, by: Duration) -> Result<(), BusError>;

    /// Installs a fault. Faults stack until [`BusControl::clear_faults`].
    ///
    /// # Errors
    /// [`BusError::Unsupported`] when the binding cannot inject this fault.
    fn set_fault(&self, fault: Fault) -> Result<(), BusError>;

    /// Removes every installed fault.
    ///
    /// # Errors
    /// [`BusError::Unsupported`] when the binding has no fault switches.
    fn clear_faults(&self) -> Result<(), BusError>;

    /// Human-readable record of each fault decision, in order. Equal seeds and
    /// equal inputs give equal traces (RFC-0011 [0011-061]).
    ///
    /// # Errors
    /// [`BusError::Unsupported`] when the binding has no fault switches.
    fn fault_trace(&self) -> Result<Vec<String>, BusError>;

    /// Simulates a bus restart: acknowledged state is kept, pending requests
    /// fail, unacknowledged deliveries are redelivered. Reopens a closed bus.
    ///
    /// # Errors
    /// [`BusError::Unsupported`] when the binding cannot simulate a restart.
    fn restart_bus(&self) -> Result<(), BusError>;

    /// Simulates a consumer crash. Its unacknowledged deliveries become
    /// redeliverable and its old handles fail. Returns `false` if the consumer
    /// does not exist.
    ///
    /// # Errors
    /// [`BusError::Unsupported`] when the binding cannot simulate a crash.
    fn crash_consumer(
        &self,
        stream: &StreamName,
        consumer: &ConsumerName,
    ) -> Result<bool, BusError>;

    /// Corrupts stored blob bytes for `digest` in `tenant`'s namespace, so the
    /// next read must report corruption. Returns `false` if the blob is absent.
    ///
    /// # Errors
    /// [`BusError::Unsupported`] when the binding cannot corrupt stored bytes.
    fn corrupt_blob(&self, tenant: &TenantId, digest: &Digest) -> Result<bool, BusError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probability_accepts_closed_unit_interval_only() {
        assert!(Probability::new(0.0).is_some());
        assert!(Probability::new(1.0).is_some());
        assert!(Probability::new(-0.001).is_none());
        assert!(Probability::new(1.001).is_none());
        assert!(Probability::new(f64::NAN).is_none());
    }
}
