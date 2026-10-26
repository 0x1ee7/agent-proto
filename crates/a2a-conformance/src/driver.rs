//! Minimal synchronous executor for the conformance suite.
//!
//! Bindings under test run on a virtual clock ([`BusControl::advance`]). When
//! a future is pending, [`drive`] moves that clock forward by one [`STEP`] and
//! polls again, so timeouts, redelivery and nak delays happen without sleeping.

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bus_api::control::BusControl;

/// Virtual time added on each pending poll.
pub const STEP: Duration = Duration::from_millis(10);

/// Pending polls allowed before [`drive`] gives up (120 s of virtual time).
pub const BUDGET_STEPS: u32 = 12_000;

/// Polls `fut` to completion, advancing the virtual clock while it is pending.
///
/// Returns `None` when the future is still pending after the budget; the
/// caller treats that as "nothing happened" (for example, no delivery).
pub fn drive<C, F>(control: &C, fut: F) -> Option<F::Output>
where
    C: BusControl + ?Sized,
    F: Future,
{
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..=BUDGET_STEPS {
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return Some(value);
        }
        control.advance(STEP);
    }
    None
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
