//! Provider ports: the three boundaries the runtime is allowed to know about.
//!
//! Everything the runtime needs from the outside is one of these three traits.
//! Nothing in `domain`, `ledger` or `kernel` mentions a filesystem, an object
//! store, a queue, a wall clock, or a vendor. That is the portability claim, and
//! it is checkable: those modules should not contain a single provider name.
//!
//! | port | what it abstracts | why it is a real boundary |
//! |---|---|---|
//! | [`DurableStore`] | where ledger bytes live and when they become durable | durability *is* the semantic; a store that cannot name its durability boundary cannot support a speculative runtime |
//! | [`Clock`] | what time is, and who advances it | time is a nondeterministic input, so it must be injectable for replay to be reproducible |
//! | [`CostModel`] | what a write and a flush cost | the runtime's performance claims are only meaningful against a measured cost, and a provider's cost is not the runtime's |
//!
//! [`crate::domain::effect::EffectSink`] is the fourth port and lives with the
//! effect model, because it is a semantic boundary rather than an infrastructure
//! one.
//!
//! Deliberately *not* ports: "queue", "cache", "leader election", "replication".
//! This runtime has exactly one durable artefact and one writer. Every additional
//! port would be an abstraction with no second implementation, which is
//! abstraction zoo rather than architecture.

use std::path::Path;
use std::time::Duration;

use crate::domain::version::LogPosition;

/// Outcome of scanning a store for a valid prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ScanReport {
    pub valid_records: usize,
    pub valid_bytes: u64,
    /// Bytes discarded because the tail was torn or corrupt.
    pub torn_bytes_dropped: u64,
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    /// The store contains bytes that are not a valid record.
    Corrupt {
        reason: String,
    },
    /// A framed record failed its checksum.
    ChecksumMismatch {
        offset: u64,
    },
    /// The store ended in the middle of a record.
    Truncated,
    /// Reading or truncating started at a byte offset that is not a record
    /// boundary.
    InvalidPosition(LogPosition),
    /// The ledger record's own encoding failed.
    Codec(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "store io error: {e}"),
            StoreError::Corrupt { reason } => write!(f, "corrupt ledger record: {reason}"),
            StoreError::ChecksumMismatch { offset } => {
                write!(f, "checksum mismatch at byte offset {offset}")
            }
            StoreError::Truncated => write!(f, "torn record at end of ledger"),
            StoreError::InvalidPosition(p) => write!(f, "{p} is not a record boundary"),
            StoreError::Codec(e) => write!(f, "ledger codec error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// Append-only durable byte storage, with an explicit durability boundary.
///
/// The contract is deliberately narrow, because every guarantee in the runtime is
/// derived from it:
///
/// * `append` buffers. A buffered record is **not** recoverable.
/// * `commit_prefix` makes *exactly* the prefix up to the given position durable.
///   This is the load-bearing method: a group commit covers a prefix, so records
///   handed over after the batch started must remain unrecoverable, or the store
///   would claim more durability than the runtime proved.
/// * `truncate_to` drops everything at or after a boundary, in both the durable
///   and the buffered region. This is what rollback uses.
/// * `recover` returns the longest valid prefix and discards a torn or corrupt
///   tail, so a process killed mid-write leaves a detectable tail rather than a
///   silently corrupt prefix.
///
/// Implementations need not be thread safe: the executor and the background
/// persister are single-threaded by construction, which is what keeps every
/// interleaving in the experiments deterministic.
pub trait DurableStore {
    /// Buffers a record and returns the byte position it *starts* at.
    fn append(&mut self, payload: Vec<u8>) -> Result<LogPosition, StoreError>;

    /// The byte position just past the last buffered record.
    ///
    /// This is what a durability boundary has to be expressed in terms of: the
    /// background writer commits the prefix that ends here. Returning the
    /// *start* of the last record from `append` and using it as a boundary would
    /// silently leave that record buffered forever — which looks exactly like
    /// correct behaviour, because the record is still there, and means the last
    /// write of every run is lost on a crash.
    fn written_end(&self) -> LogPosition;

    /// Makes every appended record durable.
    fn commit_all(&mut self) -> Result<(), StoreError>;

    /// Makes exactly the prefix up to `position` durable.
    fn commit_prefix(&mut self, position: LogPosition) -> Result<(), StoreError>;

    /// Reads every durable record at or after `position`, as raw payloads.
    fn read_from(&self, position: LogPosition) -> Result<Vec<Vec<u8>>, StoreError>;

    /// End of the durable region.
    fn sync_position(&self) -> LogPosition;

    /// End of the written region, including records not yet durable.
    fn written_position(&self) -> LogPosition;

    /// Drops everything at or after `position`.
    fn truncate_to(&mut self, position: LogPosition) -> Result<(), StoreError>;

    /// Scans for the longest valid prefix, discarding a torn or corrupt tail.
    fn recover(&mut self) -> Result<ScanReport, StoreError>;

    /// Number of durable records.
    fn durable_records(&self) -> usize;

    /// Where the store lives, if it is on disk.
    fn path(&self) -> Option<&Path>;
}

/// Time, as the runtime sees it.
///
/// The runtime advances a clock explicitly rather than reading the wall clock, for
/// one reason: **time is a nondeterministic input**. If the executor read the host
/// clock, replay would not be reproducible and the deterministic replay guarantee
/// would be false. Making the clock a port is what makes that guarantee
/// structural.
pub trait Clock {
    /// The current instant.
    fn now(&self) -> Duration;
    /// Advances to `t`.
    fn advance_to(&mut self, t: Duration);
    /// Advances by `d`.
    fn advance(&mut self, d: Duration);
}

/// The cost of persistence.
///
/// A cost model is a claim about hardware, so it belongs in a port rather than in
/// a constant. [`crate::ledger::probe::Probe`] measures the real numbers and
/// produces one of these, which is why every benchmark takes it as an input: a
/// sweep that sets the flush to zero really does remove the persistence cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostModel {
    /// Cost of writing one record: serialisation plus the write syscall.
    pub per_record: Duration,
    /// Size-proportional part of the write cost.
    pub per_kib: Duration,
    /// The durability boundary (fsync). This is the latency a synchronous runtime
    /// pays on its critical path and an asynchronous one pays in the background.
    pub flush: Duration,
}

impl CostModel {
    /// A model where persistence is free, used to isolate engine cost.
    pub const ZERO: CostModel = CostModel {
        per_record: Duration::ZERO,
        per_kib: Duration::ZERO,
        flush: Duration::ZERO,
    };

    /// Persistence whose dominant cost is the fsync.
    #[must_use]
    pub fn with_flush_latency(flush_ms: u64) -> Self {
        Self {
            per_record: Duration::from_micros(50),
            per_kib: Duration::from_micros(20),
            flush: Duration::from_millis(flush_ms),
        }
    }

    /// Cost of persisting `size` bytes: write plus flush.
    #[must_use]
    pub fn cost_of(&self, size: usize) -> Duration {
        self.write_cost_of(size) + self.flush
    }

    /// The size-dependent part of the cost, without the flush. The background
    /// persister uses this when grouping records into one batch.
    #[must_use]
    pub fn write_cost_of(&self, size: usize) -> Duration {
        self.per_record + self.per_kib * (size.div_ceil(1024) as u32)
    }

    /// Cost of a batch of `size` bytes written and then flushed **once**.
    ///
    /// This is the group-commit curve, and it is the reason group commit is
    /// nearly free on rotational storage: 128 records plus one flush cost about
    /// the same as 8 records plus one flush.
    #[must_use]
    pub fn batch_cost_of(&self, size: usize) -> Duration {
        self.write_cost_of(size) + self.flush
    }

    #[must_use]
    pub fn flush_latency(&self) -> Duration {
        self.flush
    }

    /// The same model with a different flush latency, keeping the write cost.
    #[must_use]
    pub fn scaled_flush_ms(&self, flush_ms: u64) -> Self {
        Self {
            flush: Duration::from_millis(flush_ms),
            ..*self
        }
    }
}

impl Default for CostModel {
    fn default() -> Self {
        Self::with_flush_latency(10)
    }
}

/// A deterministic virtual clock.
///
/// The executor and the background writer are modelled as two activities on one
/// shared timeline, so group commit and run-ahead are properties of the *model*
/// rather than of a measurement trick. `executor_time` is the sum of every
/// future's work — what a futures run actually spends — while `wall` is the latest
/// instant any activity reached, which is what a caller observes. Their difference
/// is the parallelism, and reporting only one of them would misrepresent the
/// runtime in one direction or the other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtualClock {
    wall: Duration,
    executor: Duration,
}

impl VirtualClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn wall(&self) -> Duration {
        self.wall
    }

    /// Sum of the simulated time every executor slot was busy.
    #[must_use]
    pub fn executor_time(&self) -> Duration {
        self.executor
    }

    /// Runs work on one slot from `start` to `start + cost`.
    pub fn work(&mut self, start: Duration, cost: Duration) {
        self.wall = self.wall.max(start.saturating_add(cost));
        self.executor = self.executor.saturating_add(cost);
    }

    /// Parks until `t`. This is the *only* way the executor is held up, so a
    /// synchronous wait is always visible as wall time that is not doing work.
    pub fn park_until(&mut self, t: Duration) {
        self.wall = self.wall.max(t);
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Duration {
        self.wall
    }

    fn advance_to(&mut self, t: Duration) {
        self.wall = self.wall.max(t);
    }

    fn advance(&mut self, d: Duration) {
        self.wall = self.wall.saturating_add(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slower_flush_costs_more() {
        let slow = CostModel::with_flush_latency(25);
        let fast = CostModel::with_flush_latency(1);
        assert!(slow.cost_of(64) > fast.cost_of(64));
        assert!(slow.write_cost_of(64) <= slow.cost_of(64));
    }

    #[test]
    fn group_commit_is_cheaper_per_record_than_individual_flushes() {
        let cost = CostModel::with_flush_latency(10);
        let individually = cost.cost_of(64) * 8;
        let grouped = cost.batch_cost_of(64 * 8);
        assert!(
            grouped < individually,
            "group={grouped:?} individual={individually:?}"
        );
    }

    #[test]
    fn scaling_the_flush_keeps_the_write_cost() {
        let c = CostModel::with_flush_latency(33);
        let scaled = c.scaled_flush_ms(4);
        assert_eq!(scaled.flush, Duration::from_millis(4));
        assert_eq!(scaled.per_record, c.per_record);
    }

    #[test]
    fn the_zero_model_is_free() {
        assert_eq!(CostModel::ZERO.cost_of(1_000_000), Duration::ZERO);
        assert_eq!(CostModel::ZERO.batch_cost_of(1_000_000), Duration::ZERO);
    }

    #[test]
    fn work_advances_the_wall_clock_and_charges_the_executor() {
        let mut c = VirtualClock::new();
        c.work(Duration::ZERO, Duration::from_millis(5));
        assert_eq!(c.wall(), Duration::from_millis(5));
        assert_eq!(c.executor_time(), Duration::from_millis(5));
        c.park_until(Duration::from_millis(9));
        assert_eq!(c.wall(), Duration::from_millis(9));
        assert_eq!(
            c.executor_time(),
            Duration::from_millis(5),
            "parking is not work"
        );
    }

    #[test]
    fn overlapping_slots_advance_the_wall_clock_faster_than_the_executor() {
        // Two futures running at once: eight units of work in four of wall time.
        let mut c = VirtualClock::new();
        c.work(Duration::ZERO, Duration::from_millis(4));
        c.work(Duration::ZERO, Duration::from_millis(4));
        assert_eq!(c.wall(), Duration::from_millis(4));
        assert_eq!(c.executor_time(), Duration::from_millis(8));
    }

    #[test]
    fn the_clock_never_moves_backwards() {
        let mut c = VirtualClock::new();
        c.advance_to(Duration::from_millis(5));
        c.advance_to(Duration::from_millis(1));
        assert_eq!(c.now(), Duration::from_millis(5));
    }
}
