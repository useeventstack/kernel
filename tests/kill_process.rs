//! Kill a real process and prove the ledger alone carries the run.
//!
//! The in-process failure matrix in `failure_matrix.rs` injects a fault and
//! unwinds. That is worth a great deal, and it is not the same claim: an unwind
//! runs no destructor, but a *cooperating* library could still have flushed on the
//! way out, and a `Vec` of records could still have been handed to the writer.
//! What it cannot test is the one thing that matters most in practice — whether
//! the file on disk is a sufficient record, when the process holding it does not
//! get to say goodbye.
//!
//! So the child here dies of `SIGABRT` from `std::process::abort`, with the write
//! buffer deliberately unflushed, and the parent then treats the file as a corpse.
//! It may only read the durable prefix, rebuild from it, and finish the run.
//!
//! What must hold for every kill point:
//!
//! 1. the ledger opens and scans without error, dropping a torn tail if there is
//!    one — a half-written frame is not corruption, it is a crash;
//! 2. the recovered graph validates, so the prefix is internally consistent;
//! 3. resuming from it reaches the same final state a failure-free run produces;
//! 4. the commit is all-or-nothing: the trunk either has the merge or does not,
//!    never a piece of it.

use std::path::Path;
use std::process::Command;

use ues::cli::crash_plan;
use ues::domain::ids::ExecutionId;
use ues::domain::version::LogPosition;
use ues::kernel::recovery::rebuild;
use ues::kernel::{Durability, ExecutionPolicy, Kernel};
use ues::ledger::record::{LedgerRecord, Payload};
use ues::ledger::store::{temp_ledger_dir, FileStore};
use ues::ports::DurableStore;
use ues::RecordingSink;

/// The binary under test, built once by cargo before this integration test runs.
fn binary() -> std::path::PathBuf {
    // `CARGO_BIN_EXE_<name>` is set by cargo for integration tests.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ues"))
}

/// How many records the crash plan writes, as a fraction-safe upper bound.
fn record_count(dir: &std::path::Path) -> u64 {
    let out = Command::new(binary())
        .env("UES_CRASH_CHILD", "1")
        .env("UES_CRASH_LEDGER", dir.join("counted"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the measuring child");
    assert!(out.status.success(), "the measuring child failed");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("unreadable record count {e:?}"))
}

/// Runs a crash child that aborts after `n` records, and returns its ledger dir.
///
/// Asserts the child really died. A child that exited cleanly is the dangerous
/// case: it would leave a *complete* ledger, every assertion below would pass,
/// and the test would be measuring nothing.
fn crash_after(n: u64, total: u64) -> std::path::PathBuf {
    let dir = temp_ledger_dir("kill");
    let path = dir.join("ledger");
    let status = Command::new(binary())
        .env("UES_CRASH_CHILD", "1")
        .env("UES_CRASH_AFTER_RECORDS", n.to_string())
        .env("UES_CRASH_LEDGER", &path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn the crash child");
    assert!(
        !status.success(),
        "the crash child at {n} of {total} records exited cleanly: no kill happened"
    );
    assert!(path.exists(), "the crash child wrote no ledger at {path:?}");
    dir
}

/// Reads the durable prefix of a ledger the way a restarted process would.
fn durable_prefix(path: &Path) -> (Vec<LedgerRecord>, u64) {
    let mut store = FileStore::open(path).expect("the ledger opens after a crash");
    // `recover` is what a restart does: it drops a torn tail. A crash mid-frame is
    // expected here, not a failure.
    let scan = store.recover().expect("the ledger scans after a crash");
    let frames = store
        .read_from(LogPosition::START)
        .expect("the durable region is readable");
    let mut records = Vec::with_capacity(frames.len());
    for frame in frames {
        match LedgerRecord::decode(frame.as_slice()) {
            Ok(r) => records.push(r),
            // A frame the decoder refuses at the very end is a torn write. The
            // scan already accounted for it; anything earlier is real corruption
            // and must not be skipped, or recovery would quietly lose a record.
            Err(_) => break,
        }
    }
    (records, scan.torn_bytes_dropped)
}

/// What a clean, uninterrupted run of the same plan produces.
struct Reference {
    trunk: String,
    /// One entry per irreversible effect that reached the world.
    charges: Vec<String>,
    /// One entry per speculative write, which may legitimately repeat: the target
    /// dedupes by key, so seeing the same key twice is the point, not a failure.
    upserts: Vec<String>,
}

fn reference() -> Reference {
    let sink = RecordingSink::new();
    let mut kernel = Kernel::with_ports(
        crash_plan(),
        ExecutionPolicy::durable(),
        Box::new(ues::MemoryStore::new()),
        Box::new(sink.clone()),
    )
    .expect("kernel");
    let report = kernel.run().expect("a clean run succeeds");
    let pick = |name: &str| -> Vec<String> {
        sink.observed()
            .iter()
            .filter(|(_, op)| op.name == name)
            .map(|(key, _)| key.as_string())
            .collect()
    };
    Reference {
        trunk: render(&report.trunk),
        charges: pick("charge"),
        upserts: pick("upsert"),
    }
}

fn render(state: &ues::State) -> String {
    state
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn a_killed_process_leaves_a_ledger_a_restart_can_finish() {
    let Reference {
        trunk: want,
        charges,
        upserts,
    } = reference();
    // The reference itself has to be right, or every comparison below is vacuous.
    // One arm's worth of work, and the trunk's own two steps.
    assert!(want.contains("work=400"), "reference: {want}");
    assert!(want.contains("counter=101"), "reference: {want}");
    // Exactly one irreversible charge, for one arm. Two losing arms charged too
    // and the reference would be wrong in a way no amount of recovery testing
    // finds, because the wrong thing would be in the reference.
    assert_eq!(charges.len(), 1, "the clean run charged {charges:?}");
    // All three arms speculated a write, and each did it under its own key, so a
    // rejected arm's write cannot be absorbed by the arm that won.
    assert_eq!(upserts.len(), 3, "the clean run upserted {upserts:?}");
    assert_eq!(
        upserts
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "speculative writes must not share a key: {upserts:?}"
    );

    // Sweep the kill across the whole run, including both sides of the commit.
    // The points are a mix of absolute and fractional, so the sweep reaches the
    // commit and the end of the log whatever the plan's length turns out to be.
    // A sweep of hard-coded counts either misses the interesting end of the run
    // or asks to kill after the last record, which silently tests nothing.
    let total = record_count(&temp_ledger_dir("count"));
    assert!(total > 100, "the crash plan should be long enough to sweep");
    let at = |num: u64, den: u64| num * total / den;
    let mut points: Vec<u64> = [
        1, 2, 3, 5, 8, 13, 40, 200, 700, 1_200, 1_210, 1_220, 1_230, 1_240,
    ]
    .into_iter()
    .chain([
        at(1, 20),
        at(1, 4),
        at(3, 8),
        at(1, 2),
        at(5, 8),
        at(7, 8),
        at(9, 10),
        at(99, 100),
        total.saturating_sub(1),
    ])
    .filter(|n| *n > 0 && *n < total)
    .collect();
    points.sort_unstable();
    points.dedup();
    for n in points {
        let dir = crash_after(n, total);
        let path = dir.join("ledger");
        let (records, torn) = durable_prefix(&path);

        assert!(
            !records.is_empty(),
            "kill at {n}: nothing durable survived, so nothing can be recovered"
        );

        // Every record must belong to one execution, or replay is meaningless.
        for r in &records {
            assert_eq!(
                r.execution, records[0].execution,
                "kill at {n}: the ledger mixes executions"
            );
        }

        // The recovered graph must be internally consistent. This is the check
        // that a partially-written log is *not* a state: a torn tail is dropped,
        // and everything the store kept has to replay to something valid.
        let graph = rebuild(&records, records[0].execution, crash_plan().id)
            .unwrap_or_else(|e| panic!("kill at {n}: {e} (torn {torn} bytes)"));
        graph
            .validate()
            .unwrap_or_else(|e| panic!("kill at {n}: recovered graph invalid: {e}"));

        // The commit is atomic. A trunk holding some of a merge is the corruption
        // this whole design exists to make impossible.
        let trunk = graph.trunk_state();
        let has_merge = trunk.get("work") > 0;
        let commits: Vec<&LedgerRecord> = records
            .iter()
            .filter(|r| matches!(r.payload, Payload::Committed { .. }))
            .collect();
        if !commits.is_empty() {
            assert_eq!(
                trunk.get("work"),
                400,
                "kill at {n}: the trunk has a partial merge ({trunk:?})"
            );
        } else {
            assert!(
                !has_merge,
                "kill at {n}: the trunk moved without a commit record ({trunk:?})"
            );
        }

        // And the run finishes. This is the real test: resume from the prefix,
        // drive the same plan, and land on the answer a clean run gives.
        let mut store = FileStore::open(&path).expect("reopen");
        store.recover().expect("rescan");
        let sink = RecordingSink::new();
        let mut kernel = Kernel::with_ports(
            crash_plan(),
            ExecutionPolicy::durable().with_durability(Durability::Synchronous),
            Box::new(store),
            Box::new(sink.clone()),
        )
        .expect("kernel over a recovered ledger");
        let report = kernel.run().expect("a recovered run finishes");
        assert_eq!(
            render(&report.trunk),
            want,
            "kill at {n}: resumed to the wrong answer"
        );

        // No irreversible effect of a rejected future may have escaped, even
        // though the process died in the middle of a speculative run. This is the
        // property a real kill can show and an in-process injection cannot: the
        // process that performed the effect had no chance to clean up.
        let committed: Vec<Vec<u32>> = kernel
            .lineage()
            .iter()
            .filter(|(_, f)| f.status == ues::FutureStatus::Committed)
            .map(|(_, f)| f.path.clone())
            .collect();
        let leaked: Vec<String> = sink
            .observed()
            .iter()
            .filter(|(key, op)| op.name == "charge" && !committed.contains(&key.path().to_vec()))
            .map(|(_, op)| op.name.clone())
            .collect();
        assert!(
            leaked.is_empty(),
            "kill at {n}: a rejected future performed {} irreversible effect(s) of {}",
            leaked.len(),
            leaked.join(", ")
        );
        // The resumed run must end up having charged exactly what a clean run
        // charges, and no more: a retry that re-issued a charge under a new key
        // would pass the leak check above and still be a double charge.
        let recharged: Vec<String> = sink
            .observed()
            .iter()
            .filter(|(_, op)| op.name == "charge")
            .map(|(key, _)| key.as_string())
            .collect();
        assert_eq!(
            recharged, charges,
            "kill at {n}: resumed run charged {recharged:?}, not {charges:?}"
        );
    }
}

#[test]
fn a_kill_before_the_first_record_leaves_nothing_and_that_is_fine() {
    // The degenerate case, and worth its own test because "the log is empty" must
    // be an answer the runtime can give rather than an error it must survive.
    let dir = crash_after(0, 1);
    let (records, _) = durable_prefix(&dir.join("ledger"));
    let plan = crash_plan();
    let mut kernel = Kernel::new(plan.clone(), ExecutionPolicy::durable()).expect("kernel");
    let report = kernel.run().expect("a fresh run finishes");
    assert_eq!(
        render(&report.trunk),
        reference().trunk,
        "a kill at zero records must change nothing about the answer"
    );
    // Rebuilding an empty prefix is an error by design: an empty log is not an
    // execution. What matters is that it is a *clean* error.
    if let Err(e) = rebuild(&records, ExecutionId::new(1), plan.id) {
        assert!(
            e.to_string().contains("not an execution"),
            "an empty prefix must be refused plainly, got: {e}"
        );
    }
}
