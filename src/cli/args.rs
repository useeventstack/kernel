//! Argument parsing and dispatch.
//!
//! Hand rolled, because the crate deliberately has no dependencies and `cargo
//! test` is meant to be fully offline and hermetic.

use std::path::PathBuf;

use crate::domain::effect::{EffectClass, EffectOp};
use crate::domain::ids::PlanId;
use crate::domain::plan::Plan;
use crate::domain::Operation;
use crate::kernel::policy::ExecutionPolicy;
use crate::kernel::{Kernel, RunReport};
use crate::ledger::store::FileStore;
use crate::ports::{CostModel, DurableStore};

/// The verbs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    /// Run a durable workflow. No options, because it needs none.
    Run {
        steps: usize,
    },
    /// Run the canonical branching demonstration.
    Demo {
        arms: usize,
        flush_ms: u64,
    },
    /// Evaluate and commit alternatives, with the durability policy spelled out.
    Explore {
        arms: usize,
        durability: String,
        flush_ms: u64,
        workers: usize,
    },
    /// Run the plan with an effect of each class, and report which escaped.
    Effects {
        flush_ms: u64,
    },
    /// Rebuild the execution graph from a ledger and print it.
    Inspect {
        path: PathBuf,
    },
    /// Run the benchmark: four arms, and what speculation costs.
    Bench,
}

/// A parsed command line.
#[derive(Clone, Debug)]
pub struct Parsed {
    pub command: Command,
}

#[must_use]
pub fn usage() -> String {
    "\
ues — a durable execution kernel with first-class execution futures

USAGE
    ues run [STEPS]              a plain durable workflow (synchronous, safe)
    ues demo [ARMS] [FLUSH_MS]    fork, evaluate, commit, recover
    ues explore [ARMS] [DURABILITY] [FLUSH_MS] [WORKERS]
                                  the same, with the durability policy chosen
    ues effects [FLUSH_MS]        which effect classes may escape speculation
    ues inspect <LEDGER>          rebuild and print an execution graph
    ues bench                     four arms, and the cost of speculation
    ues help

DURABILITY
    sync           every record is durable before the next step runs
    group-commit:N batch N records, then wait for them
    async:W        run ahead up to W records while a writer persists in groups

EXAMPLES
    ues run
    ues demo
    ues explore 4 async:8 10 4
    ues effects 10
"
    .to_owned()
}

/// Parses `argv`, which must already have the program name removed.
pub fn parse(argv: &[String]) -> Result<Parsed, String> {
    let verb = argv.first().map(String::as_str).unwrap_or("help");
    let rest: Vec<String> = argv.iter().skip(1).cloned().collect();
    let opt = |i: usize, what: &str, default: u64| -> Result<u64, String> {
        if rest.get(i).is_some() {
            number(&rest[i], what)
        } else {
            Ok(default)
        }
    };
    let command = match verb {
        "help" | "--help" | "-h" => Command::Help,
        "run" => Command::Run {
            steps: usize::try_from(opt(0, "STEPS", 20)?).unwrap_or(20),
        },
        "demo" => Command::Demo {
            arms: usize::try_from(opt(0, "ARMS", 3)?).unwrap_or(3),
            flush_ms: opt(1, "FLUSH_MS", 10)?,
        },
        "explore" => Command::Explore {
            arms: usize::try_from(opt(0, "ARMS", 3)?).unwrap_or(3),
            durability: rest.get(1).cloned().unwrap_or_else(|| "async:8".to_owned()),
            flush_ms: opt(2, "FLUSH_MS", 10)?,
            workers: usize::try_from(opt(3, "WORKERS", 3)?).unwrap_or(3).max(1),
        },
        "effects" => Command::Effects {
            flush_ms: opt(0, "FLUSH_MS", 10)?,
        },
        "bench" => {
            if !rest.is_empty() {
                return Err("bench takes no arguments".to_owned());
            }
            Command::Bench
        }
        "inspect" => {
            let path = rest
                .first()
                .ok_or_else(|| "inspect needs a ledger path".to_owned())?;
            Command::Inspect {
                path: PathBuf::from(path),
            }
        }
        other => {
            return Err(format!("unknown command `{other}`; try `ues help`"));
        }
    };
    Ok(Parsed { command })
}

fn number(s: &str, what: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .map_err(|e| format!("{what} must be a number, got `{s}`: {e}"))
}

/// Parses and executes.
pub fn run(argv: &[String]) -> Result<String, String> {
    let parsed = parse(argv)?;
    match parsed.command {
        Command::Help => Ok(usage()),
        Command::Run { steps } => render_run(steps),
        Command::Demo { arms, flush_ms } => super::render::demo(arms, flush_ms, false),
        Command::Explore {
            arms,
            durability,
            flush_ms,
            workers,
        } => super::render::explore(arms, &durability, flush_ms, workers),
        Command::Effects { flush_ms } => super::render::effects_demo(flush_ms),
        Command::Inspect { path } => inspect(&path),
        Command::Bench => Ok(crate::benchmark::report()),
    }
}

fn render_run(steps: usize) -> Result<String, String> {
    let plan = Plan::linear(
        PlanId::new(1),
        "run",
        (0..steps)
            .map(|_| Operation::Add {
                key: "counter".into(),
                by: 1,
            })
            .collect(),
    );
    let policy = ExecutionPolicy::durable();
    let line = super::render::policy_line(&policy);
    let report = execute(plan, policy, CostModel::with_flush_latency(10))?;
    let mut out = String::new();
    out.push_str("a plain durable workflow, no configuration required\n\n");
    out.push_str(&super::render::summary(&report, &line));
    Ok(out.trim_end().to_owned())
}

fn execute(plan: Plan, policy: ExecutionPolicy, cost: CostModel) -> Result<RunReport, String> {
    Kernel::new(plan, policy)
        .map_err(|e| e.to_string())?
        .with_cost(cost)
        .run()
        .map_err(|e| e.to_string())
}

fn inspect(path: &std::path::Path) -> Result<String, String> {
    use crate::domain::version::LogPosition;
    let mut store = FileStore::open(path).map_err(|e| e.to_string())?;
    let scan = store.recover().map_err(|e| e.to_string())?;
    let raw = store
        .read_from(LogPosition::START)
        .map_err(|e| e.to_string())?;
    let mut records = Vec::new();
    for bytes in raw {
        records.push(crate::LedgerRecord::decode(&bytes).map_err(|e| e.to_string())?);
    }
    let graph = crate::kernel::recovery::rebuild(
        &records,
        crate::ExecutionId::new(1),
        records
            .first()
            .and_then(|r| match r.payload {
                crate::Payload::Opened { plan } => Some(plan),
                _ => None,
            })
            .unwrap_or(PlanId::new(1)),
    )
    .map_err(|e| e.to_string())?;
    super::render::graph(&graph, scan.valid_records, scan.torn_bytes_dropped)
}

/// Builds the canonical plan: `arms` alternatives that differ logically, a
/// selection and a commit.
#[must_use]
pub fn branching_plan(arms: usize) -> Plan {
    let values: Vec<i64> = (0..arms.max(2)).map(|i| 10 * (i as i64 + 1)).collect();
    let sub = values
        .iter()
        .map(|v| {
            Plan::builder(PlanId::new(2), "arm")
                .state(Operation::Add {
                    key: "total".into(),
                    by: *v,
                })
                .state(Operation::Set {
                    key: "score".into(),
                    value: *v,
                })
                .build()
        })
        .collect();
    Plan::builder(PlanId::new(1), "branching")
        .state(Operation::Set {
            key: "total".into(),
            value: 0,
        })
        .fork("choose a plan", sub)
        .select()
        .commit()
        .state(Operation::Add {
            key: "committed".into(),
            by: 1,
        })
        .build()
}

/// A plan with one effect of every class, in an arm, so a demonstration can show
/// which of them escaped speculation.
#[must_use]
pub fn effect_plan() -> Plan {
    let arm = Plan::builder(PlanId::new(2), "arm")
        .effect(
            "read the world",
            EffectClass::Read,
            EffectOp::new("quote", 10),
            Some("quoted"),
            None,
        )
        .effect(
            "upsert",
            EffectClass::IdempotentWrite,
            EffectOp::new("upsert", 1),
            None,
            Some("upserted"),
        )
        .effect(
            "refundable charge",
            EffectClass::Compensatable,
            EffectOp::new("charge", 20),
            None,
            Some("charged"),
        )
        .effect(
            "terminal charge",
            EffectClass::Irreversible,
            EffectOp::new("charge_final", 30),
            None,
            Some("finalised"),
        )
        .state(Operation::Set {
            key: "score".into(),
            value: 1,
        })
        .build();
    Plan::builder(PlanId::new(1), "effects")
        .fork("only one arm", vec![arm.clone(), arm])
        .select()
        .commit()
        .build()
}
