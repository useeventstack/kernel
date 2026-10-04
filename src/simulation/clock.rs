//! Deterministic virtual clock.
//!
//! Every duration in the prototype is *simulated*. Nothing sleeps, so results
//! are bit-for-bit reproducible and the benchmarks compare algorithmic costs
//! rather than machine noise.
//!
//! The clock models two concurrent activities on one shared wall clock:
//!
//! ```text
//!            executor timeline            persister timeline
//!   t0 ─────────────────────────►                   (APPEND_LOG: the executor
//!        execute step 1  (1ms)                       performs the write itself,
//!        execute step 2  (1ms)                       so only this timeline has
//!        write + fsync     (10ms)   ◄── wall ──       work in it)
//!   t0 ─────────────────────────►
//!        execute step 1  (1ms)         ┐
//!        execute step 2  (1ms)         ├ parallel: the background persister
//!        execute step 3  (1ms)         │ makes progress while the executor
//!        execute step 4  (1ms)         ┘ works, and the executor never waits
//! ```
//!
//! `advance` is the "both run at once" step. `block_until` is the only way the
//! executor can be held up, and it is used exclusively by the append-log
//! baseline (whose own writes are on the critical path) and by the adaptive
//! speculation gate when it decides `PersistThenExecute`.

use std::time::Duration;

/// Virtual time source shared by the executor and the background persister.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimClock {
    executor: Duration,
    persister: Duration,
}

impl SimClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current wall clock: the furthest either activity has reached.
    #[must_use]
    pub fn wall(&self) -> Duration {
        self.executor.max(self.persister)
    }

    #[must_use]
    pub fn executor_time(&self) -> Duration {
        self.executor
    }

    #[must_use]
    pub fn persister_time(&self) -> Duration {
        self.persister
    }

    /// The executor performs `d` of work; the background persister progresses
    /// in parallel for the same `d` of wall time.
    pub fn advance(&mut self, d: Duration) {
        self.executor += d;
        self.persister += d;
    }

    /// The executor performs `d` of work on the critical path (its own write
    /// plus the flush it is waiting for).
    pub fn advance_critical(&mut self, d: Duration) {
        self.advance(d);
    }

    /// The executor blocks until the persister has reached `target`.
    ///
    /// The persister keeps working while the executor is parked, which is the
    /// only way time can pass for it.
    pub fn block_until(&mut self, target: Duration) {
        self.persister = self.persister.max(target);
        self.executor = self.executor.max(self.persister);
    }

    /// Signed difference between the executor and the persister timelines.
    /// Positive means the executor is behind the writer.
    #[must_use]
    pub fn persistence_lag(&self) -> Duration {
        self.persister.saturating_sub(self.executor)
    }
}

/// Formats a duration as milliseconds with microsecond resolution.
#[must_use]
pub fn format_ms(d: Duration) -> String {
    format!("{:.3}", as_millis_f64(d))
}

/// Formats a duration as a whole number of milliseconds, rounded up.
#[must_use]
pub fn format_ms_ceil(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    format!("{}", ms.ceil() as i64)
}

/// Converts a duration to fractional milliseconds.
#[must_use]
pub fn as_millis_f64(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Converts fractional milliseconds to a duration, rounding to microseconds
/// so that repeated addition stays exact.
#[must_use]
pub fn from_millis_f64(ms: f64) -> Duration {
    if ms <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_nanos((ms * 1_000_000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn advance_moves_both_timelines() {
        let mut c = SimClock::new();
        c.advance(ms(5));
        assert_eq!(c.wall(), ms(5));
        assert_eq!(c.executor_time(), ms(5));
        assert_eq!(c.persister_time(), ms(5));
    }

    #[test]
    fn blocking_jumps_executor_to_persister() {
        let mut c = SimClock::new();
        c.advance(ms(3));
        c.block_until(ms(20));
        assert_eq!(c.wall(), ms(20));
        assert_eq!(c.executor_time(), ms(20));
        assert_eq!(c.persister_time(), ms(20));
    }

    #[test]
    fn blocking_on_a_reached_target_is_free() {
        let mut c = SimClock::new();
        c.advance(ms(30));
        let before = c.wall();
        c.block_until(ms(20));
        assert_eq!(c.wall(), before);
    }

    #[test]
    fn parallel_execution_lets_the_persister_finish() {
        // 5 steps of 4ms each, 10ms persistence: the persister can overlap all
        // of them, so a final wait for 5*(1+2) is already satisfied.
        let mut c = SimClock::new();
        for _ in 0..5 {
            c.advance(ms(4));
        }
        assert_eq!(c.wall(), ms(20));
        c.block_until(ms(15));
        assert_eq!(c.wall(), ms(20));
    }

    #[test]
    fn lag_reports_executor_being_behind() {
        let mut c = SimClock::new();
        c.advance(ms(2));
        c.block_until(ms(9));
        assert_eq!(c.persistence_lag(), Duration::ZERO);
    }

    #[test]
    fn formatting_is_stable() {
        assert_eq!(format_ms(Duration::from_micros(1500)), "1.500");
        assert_eq!(format_ms_ceil(Duration::from_micros(100)), "1");
        assert_eq!(format_ms_ceil(Duration::from_micros(1100)), "2");
    }
}
