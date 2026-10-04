//! The background writer: a single serial resource on the shared clock.
//!
//! # What is being modelled
//!
//! There is one ledger, so there is exactly one writer. The executor submits
//! records to it and carries on; the writer picks up everything submitted before
//! it became free, writes it, and makes it durable after the flush latency. The
//! two-phase cycle is:
//!
//! ```text
//!   start     the writer is free and the clock has reached `busy_until`; it
//!             takes every record submitted at or before that instant
//!   complete  the clock reaches `durable_at`; the batch becomes recoverable
//!             and the durable frontier advances to the batch's last version
//! ```
//!
//! Because the executor never waits for the writer, the writer makes progress
//! *while the executor works*. That is the entire mechanism by which persistence
//! cost is hidden, and it is a property of the model rather than of a measurement
//! trick — the same model, with the executor forced to wait, reproduces the
//! synchronous baseline exactly.
//!
//! # Why only a prefix
//!
//! `commit_prefix` makes exactly the prefix up to a position durable. Records
//! submitted *after* the batch started stay in the write buffer and are not
//! recoverable. Without this the store would claim more durability than the
//! runtime proved, and the whole durable-prefix argument would be false.

use std::collections::VecDeque;
use std::time::Duration;

use crate::domain::version::{LogPosition, Version};
use crate::ports::CostModel;

/// A record handed to the writer.
#[derive(Clone, Copy, Debug)]
struct Submitted {
    /// Byte position just past this record's frame.
    end: LogPosition,
    size: usize,
    submitted_at: Duration,
    /// The version this record produced. Versions are globally monotonic, so the
    /// last record in a batch defines the batch's durable version.
    version: Version,
}

/// A batch the writer is currently writing.
#[derive(Clone, Copy, Debug)]
struct Writing {
    end: LogPosition,
    size: usize,
    records: usize,
    started_at: Duration,
    durable_at: Duration,
    version: Version,
}

/// One group commit.
///
/// `end` is the byte position the store must be advanced to. A batch covers a
/// *prefix*, so committing it makes exactly these records durable and leaves
/// anything appended afterwards still buffered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Batch {
    pub records: usize,
    pub bytes: usize,
    /// Byte position just past the last record in the batch.
    pub end: LogPosition,
    pub started_at: Duration,
    pub durable_at: Duration,
    /// Highest version the batch made durable.
    pub version: Version,
}

/// The virtual background writer.
#[derive(Debug)]
pub struct Writer {
    cost: CostModel,
    queue: VecDeque<Submitted>,
    writing: Option<Writing>,
    /// Simulated time the writer is busy until.
    busy_until: Duration,
    /// Last wall-clock reading the writer was polled at.
    seen: Duration,
    /// The highest version known to be durable.
    durable_version: Version,
    /// Batches completed.
    pub flushes: u64,
    /// Records made durable.
    pub durable_records: u64,
    /// Bytes made durable.
    pub durable_bytes: u64,
    /// Records submitted.
    pub submitted: u64,
    /// Synchronous waits charged to the executor.
    pub waits: u64,
    /// Total time the executor spent waiting.
    pub waited: Duration,
    /// Peak number of records in flight at once.
    pub peak_in_flight: usize,
}

impl Writer {
    #[must_use]
    pub fn new(cost: CostModel) -> Self {
        Self {
            cost,
            queue: VecDeque::new(),
            writing: None,
            busy_until: Duration::ZERO,
            seen: Duration::ZERO,
            durable_version: Version::ZERO,
            flushes: 0,
            durable_records: 0,
            durable_bytes: 0,
            submitted: 0,
            waits: 0,
            waited: Duration::ZERO,
            peak_in_flight: 0,
        }
    }

    #[must_use]
    pub fn durable_version(&self) -> Version {
        self.durable_version
    }

    /// Records that a prefix ending at `version` is already durable.
    ///
    /// Used when a kernel resumes from a recovered log: the work those records
    /// represent has already been paid for, and a writer that did not know that
    /// would re-flush from a frontier behind the store.
    pub(crate) fn note_durable(&mut self, version: Version) {
        self.durable_version = version;
    }

    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.queue.len() + usize::from(self.writing.is_some())
    }

    /// The next instant at which the writer would become free, so a caller can
    /// park until then instead of spinning.
    #[must_use]
    pub fn free_at(&self) -> Duration {
        self.writing.map_or(self.busy_until, |w| w.durable_at)
    }

    /// Submits a record to the writer.
    pub fn submit(&mut self, end: LogPosition, size: usize, at: Duration, version: Version) {
        self.queue.push_back(Submitted {
            end,
            size,
            submitted_at: at,
            version,
        });
        self.submitted += 1;
        self.peak_in_flight = self.peak_in_flight.max(self.in_flight());
    }

    /// Advances the writer to `now` and returns every batch that completed.
    ///
    /// Callers must apply the returned positions with
    /// [`crate::ports::DurableStore::commit_prefix`] in order, because a batch is
    /// only durable once the store says so.
    pub fn poll(&mut self, now: Duration) -> Vec<Batch> {
        self.seen = self.seen.max(now);
        let mut done = Vec::new();
        loop {
            if let Some(w) = self.writing {
                if self.seen < w.durable_at {
                    break;
                }
                self.writing = None;
                self.busy_until = w.durable_at;
                self.flushes += 1;
                self.durable_records += w.records as u64;
                self.durable_bytes += w.size as u64;
                self.durable_version = w.version;
                done.push(Batch {
                    records: w.records,
                    bytes: w.size,
                    end: w.end,
                    started_at: w.started_at,
                    durable_at: w.durable_at,
                    version: w.version,
                });
                continue;
            }
            if self.busy_until > self.seen {
                break; // the writer has not finished its previous batch
            }
            let start = self.seen;
            let take = self
                .queue
                .iter()
                .take_while(|r| r.submitted_at <= start)
                .count();
            if take == 0 {
                // Nothing has been handed over: the writer parks on the clock.
                self.busy_until = self.seen;
                break;
            }
            let mut size = 0usize;
            let mut end = LogPosition::START;
            let mut version = Version::ZERO;
            for _ in 0..take {
                let r = self.queue.pop_front().expect("counted above");
                size += r.size;
                end = r.end;
                version = r.version;
            }
            self.writing = Some(Writing {
                end,
                size,
                records: take,
                started_at: start,
                durable_at: start + self.cost.batch_cost_of(size),
                version,
            });
        }
        done
    }

    /// The position the writer would commit if it started a batch right now.
    /// Used to bound a synchronous commit.
    #[must_use]
    pub fn pending_end(&self) -> Option<LogPosition> {
        self.queue.back().map(|r| r.end)
    }

    /// Whether every submitted record is durable.
    #[must_use]
    pub fn is_drained(&self) -> bool {
        self.queue.is_empty() && self.writing.is_none()
    }

    /// How far the executor is ahead of durability, in records.
    #[must_use]
    pub fn lag(&self) -> usize {
        self.in_flight()
    }

    /// Forgets every record not yet made durable. Used by rollback: a speculative
    /// record that is discarded must not become durable later.
    pub fn discard_pending(&mut self, durable: Version) {
        self.queue.retain(|r| r.version <= durable);
        if self.writing.is_some_and(|w| w.version > durable) {
            self.writing = None;
        }
        self.busy_until = self.seen;
    }

    /// Anchors the writer after recovery, when the durable version is known.
    pub fn reset_to(&mut self, durable: Version, at: Duration) {
        self.queue.clear();
        self.writing = None;
        self.durable_version = durable;
        self.busy_until = at;
        self.seen = at;
    }

    /// Cost of making `size` bytes durable right now. The synchronous path uses
    /// this so the model charges the same work the async path charges in the
    /// background.
    #[must_use]
    pub fn commit_cost(&self, size: usize) -> Duration {
        self.cost.cost_of(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(flush_ms: u64) -> Writer {
        Writer::new(CostModel::with_flush_latency(flush_ms))
    }

    #[test]
    fn a_record_is_not_durable_until_the_clock_passes_the_flush() {
        let mut w = writer(10);
        w.submit(LogPosition::new(10), 40, Duration::ZERO, Version::new(1));
        // The writer picks the record up when it is first polled; the flush is
        // then charged from that instant, not from the submission.
        assert!(w.poll(Duration::ZERO).is_empty());
        assert!(!w.is_drained());
        assert!(w.poll(Duration::from_millis(5)).is_empty());
        let done = w.poll(Duration::from_millis(11));
        assert_eq!(done.len(), 1);
        assert_eq!(w.durable_version(), Version::new(1));
        assert!(w.is_drained());
    }

    #[test]
    fn records_submitted_while_busy_share_one_flush() {
        let mut w = writer(10);
        w.submit(LogPosition::new(10), 40, Duration::ZERO, Version::new(1));
        w.poll(Duration::from_millis(1));
        w.submit(
            LogPosition::new(20),
            40,
            Duration::from_millis(2),
            Version::new(2),
        );
        w.submit(
            LogPosition::new(30),
            40,
            Duration::from_millis(3),
            Version::new(3),
        );
        assert!(w.poll(Duration::from_millis(4)).is_empty());
        let done = w.poll(Duration::from_millis(20));
        assert_eq!(done.len(), 1, "the first record paid its own flush");
        assert_eq!(done[0].records, 1);
        assert_eq!(w.durable_version(), Version::new(1));
        let done = w.poll(Duration::from_millis(40));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].records, 2, "two records, one flush");
        assert_eq!(w.durable_version(), Version::new(3));
        assert_eq!(w.flushes, 2, "two group commits for three records");
    }

    #[test]
    fn a_record_submitted_after_the_batch_started_is_not_in_it() {
        let mut w = writer(10);
        w.submit(LogPosition::new(10), 40, Duration::ZERO, Version::new(1));
        w.poll(Duration::from_millis(5));
        w.submit(
            LogPosition::new(20),
            40,
            Duration::from_millis(6),
            Version::new(2),
        );
        let done = w.poll(Duration::from_millis(20));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].version, Version::new(1));
        assert_eq!(w.durable_version(), Version::new(1));
    }

    #[test]
    fn the_durable_version_never_moves_backward() {
        let mut w = writer(1);
        w.submit(LogPosition::new(10), 10, Duration::ZERO, Version::new(5));
        w.poll(Duration::ZERO);
        w.poll(Duration::from_millis(10));
        assert_eq!(w.durable_version(), Version::new(5));
        w.reset_to(Version::new(2), Duration::ZERO);
        assert_eq!(
            w.durable_version(),
            Version::new(2),
            "recovery may rewind it"
        );
    }

    #[test]
    fn discarding_pending_drops_records_the_world_will_never_see() {
        let mut w = writer(10);
        w.submit(LogPosition::new(10), 10, Duration::ZERO, Version::new(1));
        w.poll(Duration::ZERO);
        w.submit(
            LogPosition::new(20),
            10,
            Duration::from_millis(2),
            Version::new(2),
        );
        assert_eq!(w.in_flight(), 2);
        w.discard_pending(Version::new(1));
        assert_eq!(w.in_flight(), 1, "only the speculative record is dropped");
        // The surviving batch is V1's, and V2 never becomes durable.
        let done = w.poll(Duration::from_millis(100));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].version, Version::new(1));
        assert_eq!(w.durable_version(), Version::new(1));
    }

    #[test]
    fn lag_and_peak_track_run_ahead() {
        let mut w = writer(10);
        for v in 1..=5u64 {
            w.submit(
                LogPosition::new(v * 10),
                10,
                Duration::from_millis(v),
                Version::new(v),
            );
        }
        assert_eq!(w.in_flight(), 5);
        assert_eq!(w.peak_in_flight, 5);
        assert_eq!(w.lag(), 5);
    }
}
