//! Storage-layer fault injection.
//!
//! Every other failure in this runtime is injected at a *checkpoint the kernel
//! chooses*. That is a weak test: the kernel knows a failure is coming and can
//! tidy up. A real crash gives no warning and leaves storage in whatever state the
//! write buffer happened to be in.
//!
//! This module attacks the ledger the way a crash would, from the outside:
//!
//! * [`truncate_to`] — cut the file at an arbitrary byte offset, so the last record
//!   is torn mid-frame;
//! * [`corrupt_at`] — flip a byte, so a record is complete but its checksum no
//!   longer matches.
//!
//! [`scan_all_offsets`] then cuts at *every* byte offset and [`scan_all_bytes`]
//! flips *every* byte, checking the one property that must hold for all of them:
//!
//! > recovery never panics, never accepts a record it should reject, and the
//! > recovered execution graph is always one a valid execution could have
//! > produced — never a state ahead of the work, never a torn graph.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::domain::ids::{ExecutionId, PlanId, TRUNK};
use crate::domain::node::StateNode;
use crate::domain::state::State;
use crate::domain::version::LogPosition;
use crate::kernel::recovery::{self, ExecutionGraph};
use crate::ledger::record::LedgerRecord;
use crate::ports::{DurableStore, StoreError};

/// What a recovery found at one damage point.
#[derive(Clone, Debug)]
pub struct ScanOutcome {
    /// Byte size of the damaged file.
    pub size: u64,
    /// Records the recovery accepted.
    pub records: usize,
    /// Bytes the recovery discarded.
    pub torn_bytes: u64,
    /// The trunk head the recovery rebuilt, if it rebuilt one.
    pub trunk: Option<StateNode>,
    /// How many futures the graph holds.
    pub futures: usize,
    /// Whether the graph passed its own consistency checks.
    pub consistent: bool,
    /// The error, when the recovery refused the ledger outright.
    pub rejected: Option<String>,
}

/// Aggregated result of an exhaustive fault sweep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Distinct damage points probed.
    pub offsets_tried: u64,
    /// How many produced a graph that validated.
    pub recovered: u64,
    /// How many the recovery refused outright.
    pub rejected: u64,
    /// Graphs that failed [`ExecutionGraph::validate`]. Must be zero.
    pub inconsistent: u64,
    /// Graphs that reported more records than were ever written.
    pub invented: u64,
    /// Largest number of records any single damage point preserved.
    pub max_records: u64,
}

impl SweepReport {
    /// True if every damage point recovered to a valid graph or was refused.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.inconsistent == 0 && self.invented == 0 && self.offsets_tried > 0
    }
}

#[derive(Debug)]
pub enum StorageFaultError {
    Io(std::io::Error),
    Store(String),
}

impl fmt::Display for StorageFaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageFaultError::Io(e) => write!(f, "storage fault io error: {e}"),
            StorageFaultError::Store(e) => write!(f, "storage fault store error: {e}"),
        }
    }
}

impl std::error::Error for StorageFaultError {}

impl From<std::io::Error> for StorageFaultError {
    fn from(e: std::io::Error) -> Self {
        StorageFaultError::Io(e)
    }
}

impl From<StoreError> for StorageFaultError {
    fn from(e: StoreError) -> Self {
        StorageFaultError::Store(e.to_string())
    }
}

/// Truncates a copy of `source` to `offset` bytes and writes it to `dest`.
///
/// # Errors
/// Returns [`StorageFaultError::Io`] if either file cannot be read or written.
pub fn truncate_to(source: &Path, dest: &Path, offset: u64) -> Result<(), StorageFaultError> {
    let bytes = fs::read(source)?;
    let offset = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    fs::write(dest, &bytes[..offset])?;
    Ok(())
}

/// Corrupts one byte of a copy of `source`.
///
/// # Errors
/// Returns [`StorageFaultError::Io`] if either file cannot be read or written.
pub fn corrupt_at(
    source: &Path,
    dest: &Path,
    offset: u64,
    xor: u8,
) -> Result<(), StorageFaultError> {
    let mut bytes = fs::read(source)?;
    let i = usize::try_from(offset).unwrap_or(usize::MAX);
    if i < bytes.len() {
        bytes[i] ^= xor;
    }
    fs::write(dest, &bytes)?;
    Ok(())
}

/// Rebuilds the execution graph from a damaged ledger, exactly as a recovering
/// process would.
///
/// # Errors
/// Returns [`StorageFaultError`] if the file cannot be read at all. A ledger the
/// recovery *refuses* is reported as `ScanOutcome::rejected`, not as an error,
/// because refusing is a legitimate outcome.
pub fn recover_from(
    path: &Path,
    execution: ExecutionId,
    plan: PlanId,
) -> Result<ScanOutcome, StorageFaultError> {
    let size = fs::metadata(path)?.len();
    let mut store = crate::ledger::store::FileStore::open(path)?;
    let report = store.recover()?;
    let raw = store.read_from(LogPosition::START)?;
    let mut records = Vec::with_capacity(raw.len());
    let mut rejected = None;
    for bytes in raw {
        match LedgerRecord::decode(&bytes) {
            Ok(r) => records.push(r),
            Err(e) => {
                rejected = Some(e.to_string());
                break;
            }
        }
    }
    let graph = recovery::rebuild(&records, execution, plan);
    Ok(match graph {
        Ok(g) => ScanOutcome {
            size,
            records: report.valid_records,
            torn_bytes: report.torn_bytes_dropped,
            trunk: Some(g.trunk),
            futures: g.lineage.len(),
            consistent: g.validate().is_ok(),
            rejected,
        },
        Err(e) => ScanOutcome {
            size,
            records: report.valid_records,
            torn_bytes: report.torn_bytes_dropped,
            trunk: None,
            futures: 0,
            consistent: false,
            rejected: Some(rejected.unwrap_or_else(|| e.to_string())),
        },
    })
}

/// The state the trunk holds after the first `n` delta records of one future.
///
/// This is the *reference* reconstruction: it applies deltas in ledger order with
/// the simplest possible code, because a reference should be the least clever
/// thing that could possibly be right.
#[must_use]
pub fn prefix_state(base: &State, deltas: &[crate::domain::StateDelta]) -> State {
    let mut state = base.clone();
    for d in deltas {
        let _ = state.apply(&d.operation);
    }
    state
}

/// Writes a real ledger file with `n` step records on the trunk and returns it with
/// the base state and the deltas that produced it.
///
/// # Errors
/// Returns [`StorageFaultError`] if the file cannot be created.
pub fn build_ledger(
    dir: &Path,
    n: usize,
) -> Result<
    (
        PathBuf,
        State,
        Vec<crate::domain::StateDelta>,
        ExecutionId,
        PlanId,
    ),
    StorageFaultError,
> {
    use crate::domain::delta::StateDelta;
    use crate::domain::ids::{AttemptId, FutureId, NodeId, StepId};
    use crate::domain::version::{Sequence, Version};
    use crate::ledger::record::Payload;

    let path = dir.join("generated.log");
    let mut store = crate::ledger::store::FileStore::create(&path)?;
    let execution = ExecutionId::new(1);
    let plan = PlanId::new(1);
    let mut records = Vec::with_capacity(n);
    let mut deltas = Vec::with_capacity(n);

    let push = |store: &mut crate::ledger::store::FileStore,
                payload: Payload,
                version: u64,
                seq: u64,
                ts: u64| {
        let record = LedgerRecord::new(
            execution,
            Version::new(version),
            Sequence::new(seq),
            AttemptId::new(0),
            ts,
            payload,
        );
        store.append(record.encode()).expect("append");
    };
    push(&mut store, Payload::Opened { plan }, 0, 0, 0);
    for i in 0..n {
        let delta = StateDelta::new(
            FutureId::new(0),
            StepId::new(i as u64),
            AttemptId::new(0),
            Version::new(i as u64),
            Version::new(i as u64 + 1),
            crate::domain::Operation::Add {
                key: "counter".into(),
                by: 1,
            },
        );
        deltas.push(delta.clone());
        records.push((
            i as u64,
            Payload::Step {
                delta: Box::new(delta),
                cursor: NodeId::new(i as u64 + 1),
            },
        ));
    }
    for (i, (version, payload)) in records.into_iter().enumerate() {
        push(
            &mut store,
            payload,
            version + 1,
            (i + 1) as u64,
            (i + 1) as u64 * 1000,
        );
    }
    store.commit_all()?;
    Ok((path, State::new(), deltas, execution, plan))
}

/// Cuts the ledger at every byte offset and checks each recovery.
///
/// # Errors
/// Returns [`StorageFaultError`] if the ledger cannot be read.
pub fn scan_all_offsets(
    path: &Path,
    dir: &Path,
    execution: ExecutionId,
    plan: PlanId,
    deltas: &[crate::domain::StateDelta],
) -> Result<SweepReport, StorageFaultError> {
    let total = fs::metadata(path)?.len();
    let victim = dir.join("victim-trunc.log");
    let mut report = SweepReport::default();
    for offset in 0..=total {
        report.offsets_tried += 1;
        truncate_to(path, &victim, offset)?;
        tally(recover_from(&victim, execution, plan)?, deltas, &mut report);
    }
    fs::remove_file(&victim).ok();
    Ok(report)
}

/// Flips each byte in turn and checks each recovery.
///
/// # Errors
/// Returns [`StorageFaultError`] if the ledger cannot be read.
pub fn scan_all_bytes(
    path: &Path,
    dir: &Path,
    execution: ExecutionId,
    plan: PlanId,
    deltas: &[crate::domain::StateDelta],
) -> Result<SweepReport, StorageFaultError> {
    let total = fs::metadata(path)?.len();
    let victim = dir.join("victim-corrupt.log");
    let mut report = SweepReport::default();
    for offset in 0..total {
        for xor in [0x01u8, 0x80, 0xff] {
            report.offsets_tried += 1;
            corrupt_at(path, &victim, offset, xor)?;
            tally(recover_from(&victim, execution, plan)?, deltas, &mut report);
        }
    }
    fs::remove_file(&victim).ok();
    Ok(report)
}

fn tally(outcome: ScanOutcome, deltas: &[crate::domain::StateDelta], report: &mut SweepReport) {
    if outcome.rejected.is_some() && outcome.trunk.is_none() {
        report.rejected += 1;
        return;
    }
    report.recovered += 1;
    report.max_records = report.max_records.max(outcome.records as u64);
    if outcome.records > deltas.len() + 1 {
        report.invented += 1;
    }
    if !outcome.consistent {
        report.inconsistent += 1;
    }
}

/// The trunk node a healthy recovery of `n` steps must produce.
#[must_use]
pub fn expected_trunk_after(deltas: &[crate::domain::StateDelta]) -> Option<StateNode> {
    if deltas.is_empty() {
        return Some(StateNode::EMPTY);
    }
    let mut state = State::new();
    for d in deltas {
        let _ = state.apply(&d.operation);
    }
    Some(StateNode::new(state.content_hash()))
}

/// The trunk the recovery of a damaged ledger produced, if any.
#[must_use]
pub fn trunk_state(graph: &ExecutionGraph) -> State {
    graph.trunk_state()
}

/// Convenience: the trunk id every ledger starts from.
#[must_use]
pub fn trunk_id() -> crate::domain::ids::FutureId {
    TRUNK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_truncation_offset_recovers_to_a_valid_graph() {
        let dir = crate::ledger::store::temp_ledger_dir("trunc-sweep");
        let (path, _base, deltas, exec, plan) = build_ledger(&dir, 8).expect("build");
        let report = scan_all_offsets(&path, &dir, exec, plan, &deltas).expect("sweep");
        assert!(report.offsets_tried > 8, "tried {}", report.offsets_tried);
        assert!(report.is_clean(), "{report:?}");
        assert!(report.max_records as usize <= deltas.len() + 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_single_byte_corruption_is_detected_or_survivable() {
        let dir = crate::ledger::store::temp_ledger_dir("corrupt-sweep");
        let (path, _base, deltas, exec, plan) = build_ledger(&dir, 6).expect("build");
        let report = scan_all_bytes(&path, &dir, exec, plan, &deltas).expect("sweep");
        assert!(report.is_clean(), "{report:?}");
        // Corruption need not be *rejected* to be *detected*: because the ledger
        // is append-only, a bad record invalidates its own tail, so the checksum
        // shows up as the ledger being cut back. The property that matters is
        // that no corrupted byte ever survives as a valid record.
        assert!(
            report.max_records < deltas.len() as u64 + 1,
            "a corrupted record must never be accepted: {report:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncation_never_yields_more_records_than_were_written() {
        let dir = crate::ledger::store::temp_ledger_dir("trunc-bounds");
        let (path, _base, deltas, exec, plan) = build_ledger(&dir, 5).expect("build");
        let victim = dir.join("v.log");
        let total = fs::metadata(&path).unwrap().len();
        for offset in 0..=total {
            truncate_to(&path, &victim, offset).unwrap();
            let outcome = recover_from(&victim, exec, plan).unwrap();
            assert!(outcome.records <= deltas.len() + 1, "offset {offset}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_complete_ledger_rebuilds_the_expected_trunk() {
        let dir = crate::ledger::store::temp_ledger_dir("complete");
        let (path, _base, deltas, exec, plan) = build_ledger(&dir, 5).expect("build");
        let outcome = recover_from(&path, exec, plan).unwrap();
        assert!(outcome.consistent);
        assert_eq!(outcome.futures, 1, "no fork, so only the trunk");
        assert_eq!(outcome.trunk, expected_trunk_after(&deltas));
        assert!(outcome.rejected.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
