//! Minimal synchronous executor and time source for the conformance suite.
//!
//! [`Timeline`] is the only way the suite moves time or touches the control
//! surface. In [`Clock::Virtual`] mode it drives the binding's virtual clock,
//! so timeouts, redelivery and nak delays happen without sleeping. In
//! [`Clock::Real`] mode the suite waits on the wall clock with every nominal
//! duration multiplied by the factory's time scale; this is for bindings with
//! a real clock. A control operation the binding does not provide becomes an
//! error starting with [`UNSUPPORTED`], which the suite reports as an
//! unexercised row.

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use a2a_types::ids::TenantId;
use bus_api::control::{BusControl, Fault};
use bus_api::error::BusError;
use bus_api::name::{ConsumerName, StreamName};
use bus_api::value::Digest;

/// Virtual time added on each pending poll.
pub const STEP: Duration = Duration::from_millis(10);

/// Pending polls allowed before [`drive`] gives up (120 s of nominal time).
pub const BUDGET_STEPS: u32 = 12_000;

/// Smallest real sleep between polls in real-clock mode.
pub const REAL_MIN_STEP: Duration = Duration::from_millis(1);

/// Prefix of the error text for a control operation the binding does not
/// provide, followed by the capability name.
pub const UNSUPPORTED: &str = "unsupported: ";

/// How time passes while the suite runs against a binding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Clock {
    /// The binding's virtual clock moves only through [`BusControl::advance`].
    Virtual,
    /// The wall clock. Nominal durations are multiplied by `scale`, which lies
    /// in (0, 1].
    Real {
        /// Multiplier applied to every nominal suite duration.
        scale: f64,
    },
}

impl Clock {
    /// Builds a real clock.
    ///
    /// # Errors
    /// Returns a message when `scale` is NaN, zero, negative or above one.
    pub fn real(scale: f64) -> Result<Self, String> {
        if scale.is_finite() && scale > 0.0 && scale <= 1.0 {
            Ok(Self::Real { scale })
        } else {
            Err(format!("time scale {scale} must be in (0, 1]"))
        }
    }

    /// Converts a nominal suite duration to this clock.
    #[must_use]
    pub fn scaled(self, nominal: Duration) -> Duration {
        match self {
            Self::Virtual => nominal,
            Self::Real { scale } => nominal.mul_f64(scale),
        }
    }
}

/// Time and control access for one binding under test.
#[derive(Debug)]
pub struct Timeline<'a, C: BusControl + ?Sized> {
    control: &'a C,
    clock: Clock,
    started: Instant,
}

impl<'a, C: BusControl + ?Sized> Timeline<'a, C> {
    /// Wraps `control`; real-clock time is measured from this call.
    pub fn new(control: &'a C, clock: Clock) -> Self {
        Self {
            control,
            clock,
            started: Instant::now(),
        }
    }

    /// The clock this timeline runs on.
    #[must_use]
    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// Elapsed time: virtual time, or wall time since [`Timeline::new`].
    ///
    /// # Errors
    /// `unsupported: now` when a virtual binding reports no clock.
    pub fn now(&self) -> Result<Duration, String> {
        match self.clock {
            Clock::Virtual => self.control.now().map_err(|e| control_error("now", e)),
            Clock::Real { .. } => Ok(self.started.elapsed()),
        }
    }

    /// Moves time forward by `by`: advances the virtual clock, or sleeps. On a real
    /// clock `by` is wall time, slept as given; callers pass durations already scaled
    /// by [`Clock::scaled`].
    ///
    /// # Errors
    /// `unsupported: advance` when a virtual binding has no clock to advance.
    pub fn advance(&self, by: Duration) -> Result<(), String> {
        match self.clock {
            Clock::Virtual => self
                .control
                .advance(by)
                .map_err(|e| control_error("advance", e)),
            Clock::Real { .. } => {
                std::thread::sleep(by);
                Ok(())
            }
        }
    }

    /// Moves time forward by one poll interval.
    fn step(&self) -> Result<(), String> {
        match self.clock {
            Clock::Virtual => self.advance(STEP),
            Clock::Real { scale } => {
                std::thread::sleep(STEP.mul_f64(scale).max(REAL_MIN_STEP));
                Ok(())
            }
        }
    }

    /// Installs a fault.
    ///
    /// # Errors
    /// `unsupported: <fault>` when the binding cannot inject it.
    pub fn set_fault(&self, fault: Fault) -> Result<(), String> {
        self.control
            .set_fault(fault)
            .map_err(|e| control_error("set_fault", e))
    }

    /// Removes every installed fault.
    ///
    /// # Errors
    /// `unsupported: clear_faults` when the binding has no fault switches.
    pub fn clear_faults(&self) -> Result<(), String> {
        self.control
            .clear_faults()
            .map_err(|e| control_error("clear_faults", e))
    }

    /// The fault decision trace.
    ///
    /// # Errors
    /// `unsupported: fault_trace` when the binding has no fault switches.
    pub fn fault_trace(&self) -> Result<Vec<String>, String> {
        self.control
            .fault_trace()
            .map_err(|e| control_error("fault_trace", e))
    }

    /// Simulates a bus restart.
    ///
    /// # Errors
    /// `unsupported: restart_bus` when the binding cannot restart.
    pub fn restart_bus(&self) -> Result<(), String> {
        self.control
            .restart_bus()
            .map_err(|e| control_error("restart_bus", e))
    }

    /// Simulates a consumer crash; `false` when the consumer does not exist.
    ///
    /// # Errors
    /// `unsupported: crash_consumer` when the binding cannot crash consumers.
    pub fn crash_consumer(
        &self,
        stream: &StreamName,
        consumer: &ConsumerName,
    ) -> Result<bool, String> {
        self.control
            .crash_consumer(stream, consumer)
            .map_err(|e| control_error("crash_consumer", e))
    }

    /// Corrupts stored blob bytes; `false` when the blob is absent.
    ///
    /// # Errors
    /// `unsupported: corrupt_blob` when the binding cannot corrupt bytes.
    pub fn corrupt_blob(&self, tenant: &TenantId, digest: &Digest) -> Result<bool, String> {
        self.control
            .corrupt_blob(tenant, digest)
            .map_err(|e| control_error("corrupt_blob", e))
    }
}

/// Text for a failed control operation. Unsupported operations keep the
/// [`UNSUPPORTED`] marker so the caller can tell them from real failures.
fn control_error(op: &str, error: BusError) -> String {
    match error {
        BusError::Unsupported { capability } => format!("{UNSUPPORTED}{capability}"),
        other => format!("control {op} failed: {other}"),
    }
}

/// Polls `fut` to completion, moving time forward by one step while it is
/// pending.
///
/// Returns `Ok(None)` when the future is still pending after the budget; the
/// caller treats that as "nothing happened" (for example, no delivery).
///
/// # Errors
/// Returns the message of a control operation that failed while time moved.
pub fn drive<C, F>(timeline: &Timeline<'_, C>, fut: F) -> Result<Option<F::Output>, String>
where
    C: BusControl + ?Sized,
    F: Future,
{
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..=BUDGET_STEPS {
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return Ok(Some(value));
        }
        timeline.step()?;
    }
    Ok(None)
}

/// Polls `fut` once without advancing the clock.
///
/// Returns `None` when the future is pending; used to prove that something
/// does not happen yet.
pub fn ready_now<F: Future>(fut: F) -> Option<F::Output> {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

/// Runs two futures concurrently on the current task and returns both results.
pub async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = pin!(a);
    let mut b = pin!(b);
    let mut ra: Option<A::Output> = None;
    let mut rb: Option<B::Output> = None;
    poll_fn(move |cx| {
        if ra.is_none() {
            if let Poll::Ready(value) = a.as_mut().poll(cx) {
                ra = Some(value);
            }
        }
        if rb.is_none() {
            if let Poll::Ready(value) = b.as_mut().poll(cx) {
                rb = Some(value);
            }
        }
        match (ra.take(), rb.take()) {
            (Some(x), Some(y)) => Poll::Ready((x, y)),
            (x, y) => {
                ra = x;
                rb = y;
                Poll::Pending
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bus_api::name::{ConsumerName, StreamName};
    use bus_api::value::Digest;
    use std::task::Poll;

    /// A binding with no clock and no fault switches.
    struct NoControl;

    impl BusControl for NoControl {
        fn seed(&self) -> u64 {
            0
        }
        fn now(&self) -> Result<Duration, BusError> {
            Err(BusError::Unsupported { capability: "now" })
        }
        fn advance(&self, _by: Duration) -> Result<(), BusError> {
            Err(BusError::Unsupported {
                capability: "advance",
            })
        }
        fn set_fault(&self, _fault: Fault) -> Result<(), BusError> {
            Err(BusError::Unsupported { capability: "drop" })
        }
        fn clear_faults(&self) -> Result<(), BusError> {
            Err(BusError::Unsupported {
                capability: "clear_faults",
            })
        }
        fn fault_trace(&self) -> Result<Vec<String>, BusError> {
            Err(BusError::Unsupported {
                capability: "fault_trace",
            })
        }
        fn restart_bus(&self) -> Result<(), BusError> {
            Err(BusError::Unsupported {
                capability: "restart_bus",
            })
        }
        fn crash_consumer(
            &self,
            _stream: &StreamName,
            _consumer: &ConsumerName,
        ) -> Result<bool, BusError> {
            Err(BusError::Unsupported {
                capability: "crash_consumer",
            })
        }
        fn corrupt_blob(&self, _tenant: &TenantId, _digest: &Digest) -> Result<bool, BusError> {
            Err(BusError::Unsupported {
                capability: "corrupt_blob",
            })
        }
    }

    // [0011-080] real-clock scale is validated; zero, negative, NaN and
    // values above one are rejected.
    #[test]
    fn req_0011_080_real_clock_scale_must_be_in_unit_interval() {
        assert!(Clock::real(0.0).is_err());
        assert!(Clock::real(-0.5).is_err());
        assert!(Clock::real(f64::NAN).is_err());
        assert!(Clock::real(1.5).is_err());
        assert_eq!(Clock::real(0.5), Ok(Clock::Real { scale: 0.5 }));
    }

    // [0011-080] real-clock mode scales nominal suite durations; virtual mode
    // leaves them unchanged.
    #[test]
    fn req_0011_080_real_clock_scales_nominal_durations() {
        let two = Duration::from_secs(2);
        assert_eq!(Clock::Virtual.scaled(two), two);
        assert_eq!(
            Clock::Real { scale: 0.5 }.scaled(two),
            Duration::from_secs(1)
        );
    }

    // [0011-080] real-clock drive waits between polls on the wall clock, so a
    // future that needs three pending polls takes at least three minimum steps.
    #[test]
    fn req_0011_080_real_clock_drive_waits_between_polls() {
        let control = NoControl;
        let timeline = Timeline::new(&control, Clock::Real { scale: 0.01 });
        let mut polls = 0_u32;
        let fut = std::future::poll_fn(|_cx| {
            polls += 1;
            if polls > 3 {
                Poll::Ready(polls)
            } else {
                Poll::Pending
            }
        });
        let started = Instant::now();
        let out = drive(&timeline, fut);
        assert_eq!(out, Ok(Some(4)));
        assert!(started.elapsed() >= 3 * REAL_MIN_STEP);
    }

    // [0011-082] a control operation the binding does not provide is reported
    // with the UNSUPPORTED marker, so the suite can mark the row unexercised
    // instead of failing it.
    #[test]
    fn req_0011_082_unsupported_control_maps_to_marker() -> Result<(), String> {
        let control = NoControl;
        let timeline = Timeline::new(&control, Clock::Virtual);
        let tenant = TenantId::parse("local").map_err(|e| e.to_string())?;
        let probability =
            bus_api::control::Probability::new(0.5).ok_or("probability 0.5 is valid")?;
        let fault = Fault::Drop(probability);
        assert_eq!(timeline.set_fault(fault), Err(format!("{UNSUPPORTED}drop")));
        assert_eq!(
            timeline.restart_bus(),
            Err(format!("{UNSUPPORTED}restart_bus"))
        );
        assert_eq!(
            timeline.corrupt_blob(&tenant, &Digest::from_bytes([0_u8; 32])),
            Err(format!("{UNSUPPORTED}corrupt_blob"))
        );
        Ok(())
    }
}
