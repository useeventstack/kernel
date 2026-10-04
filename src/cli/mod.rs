//! The command-line surface.
//!
//! Deliberately small: four verbs, and the default one takes no configuration at
//! all. A developer who has heard of speculative execution should be able to run a
//! durable workflow without seeing any of it; the advanced verbs exist for the
//! people who want it.

pub mod args;
mod render;

pub use args::{usage, Command, Parsed};
pub use render::DemoReport;

/// Entry point used by `main`.
pub fn main_with_args(argv: &[String]) -> u8 {
    match args::run(argv) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(message) => {
            eprintln!("error: {message}");
            1
        }
    }
}

/// The crash hook used by the process-kill test.
///
/// Dispatch is environment-only, never the command line, so it cannot be reached
/// by a typo and never appears in `--help`. The child dies of `SIGABRT` via
/// [`std::process::abort`]: for every property the test depends on, `SIGABRT` and
/// `SIGKILL` are equivalent — neither can be caught, blocked or ignored, neither
/// unwinds, neither runs a destructor, and neither flushes on the way out.
/// `SIGKILL` is not used because issuing it from Rust without a `libc` dependency
/// means spawning an external process, which is slower than the run itself and
/// risks landing on a recycled pid.
///
/// Returns normally when the environment does not ask for a crash, so it can sit
/// at the top of `main` as a single unconditional line.
pub fn crash_child() {
    if std::env::var_os("UES_CRASH_CHILD").is_none() {
        return;
    }
    match std::env::var_os("UES_CRASH_AFTER_RECORDS") {
        Some(after) => {
            let n: u64 = after
                .to_string_lossy()
                .parse()
                .expect("UES_CRASH_AFTER_RECORDS must be a number");
            crash_after_records(n);
        }
        None => {
            // Asked to be a crash child but not told when to die: measure instead.
            // The record count a plan produces depends on its cost model and its
            // durability policy, so a test that hard-codes kill points either
            // misses the interesting end of the run or asks to kill after the last
            // record. Reporting the length lets the sweep be a fraction of the
            // real thing.
            let total = crash_report();
            println!("{total}");
            std::process::exit(0);
        }
    }
}

/// Runs `plan` writing to the ledger, aborting once it has written `n` records.
///
/// The plan branches, selects and commits on purpose. A linear plan proves only
/// that replay works; the properties worth killing a process for are the ones on
/// either side of the atomic commit point, and a run that never commits cannot
/// show them.
fn crash_after_records(n: u64) -> ! {
    use crate::kernel::{ExecutionPolicy, Kernel};
    use crate::ledger::store::FileStore;

    let path = std::env::var_os("UES_CRASH_LEDGER")
        .map_or_else(|| "crash.ledger".into(), std::ffi::OsString::from);
    let store = FileStore::create(std::path::Path::new(&path)).expect("create ledger");
    let plan = crash_plan();
    let mut kernel = Kernel::with_ports(
        plan,
        // A window larger than the plan, so nothing is forced durable until the
        // end. A kill therefore lands in the middle of genuinely speculative work
        // rather than at a convenient flush.
        ExecutionPolicy::speculative(1_000_000),
        Box::new(store),
        Box::new(crate::RecordingSink::new()),
    )
    .expect("kernel");
    loop {
        match kernel.step_once() {
            Ok(true) => {
                if kernel.records().len() as u64 >= n {
                    // The write buffer is *not* flushed and nothing unwinds, so
                    // the file is left exactly as a crash would leave it.
                    std::process::abort();
                }
            }
            Ok(false) => break,
            Err(e) => panic!("crash child failed: {e}"),
        }
    }
    std::process::exit(0);
}

/// Runs the plan to completion and returns how many records it wrote.
///
/// Deliberately not the same durability policy as the crashing child: this is a
/// measurement, and the count it reports is an upper bound for any policy that
/// writes at least one record per node.
fn crash_report() -> u64 {
    use crate::kernel::{ExecutionPolicy, Kernel};
    use crate::ledger::store::FileStore;
    let path = std::env::var_os("UES_CRASH_LEDGER")
        .map_or_else(|| "crash.ledger".into(), std::ffi::OsString::from);
    let store = FileStore::create(std::path::Path::new(&path)).expect("create ledger");
    let mut kernel = Kernel::with_ports(
        crash_plan(),
        ExecutionPolicy::speculative(1_000_000),
        Box::new(store),
        Box::new(crate::RecordingSink::new()),
    )
    .expect("kernel");
    while kernel.step_once().expect("step") {}
    kernel.records().len() as u64
}

/// The plan the crash child runs.
///
/// Public so the test replays *this* plan rather than a copy of it. A test that
/// rebuilt the plan itself would be testing a plan that merely looks like the one
/// that crashed, and the first divergence would be invisible.
#[must_use]
pub fn crash_plan() -> crate::domain::plan::Plan {
    use crate::domain::effect::{EffectClass, EffectOp};
    use crate::domain::ids::PlanId;
    use crate::domain::plan::Plan;
    use crate::Operation as Op;
    let arm = |v: i64, len: usize| {
        Plan::builder(PlanId::new(2), "arm")
            .effect(
                "read",
                EffectClass::Read,
                EffectOp::new("quote", v),
                Some("quoted"),
                None,
            )
            // Speculated on: idempotent, so the target dedupes it by key.
            .effect(
                "upsert",
                EffectClass::IdempotentWrite,
                EffectOp::new("upsert", v),
                None,
                Some("upserted"),
            )
            .state(Op::Set {
                key: "score".into(),
                value: v,
            })
            .state(Op::Add {
                key: "work".into(),
                by: 1,
            })
            .repeated(len - 1)
            // Deferred to commit: the property a kill most needs to prove is that
            // this one never escapes for a future that did not commit.
            .effect(
                "charge",
                EffectClass::Irreversible,
                EffectOp::new("charge", v),
                None,
                Some("charged"),
            )
            .build()
    };
    Plan::builder(PlanId::new(1), "crash")
        .state(Op::Add {
            key: "counter".into(),
            by: 1,
        })
        .fork("choose", vec![arm(1, 400), arm(2, 400), arm(3, 400)])
        .select()
        .commit()
        .state(Op::Add {
            key: "counter".into(),
            by: 100,
        })
        .build()
}
