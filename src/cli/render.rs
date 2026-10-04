//! Rendering: the human-facing answers.
//!
//! Every function here exists because "show me what happened" is a real product
//! requirement, not because output is easier to write than a type. The formats are
//! plain text on purpose — a JSON blob is a machine interface, and this is a
//! research prototype whose output is read by a person deciding whether the idea
//! works.

use std::fmt::Write as _;
use std::time::Duration;

use crate::domain::effect::RecordingSink;
use crate::domain::future::FutureStatus;
use crate::domain::ids::TRUNK;
use crate::kernel::policy::Durability;
use crate::kernel::recovery::ExecutionGraph;
use crate::kernel::{ExecutionPolicy, Kernel, RunReport};
use crate::ports::CostModel;

/// A demonstration run, with the pieces a reader needs to judge it.
pub struct DemoReport {
    pub report: RunReport,
    /// The policy, pre-rendered: a policy owns a boxed evaluator, so it is not
    /// `Clone` and a report cannot hold a copy of it.
    pub policy: String,
    pub sink: RecordingSink,
    pub cost: CostModel,
}

/// Renders a policy for a report.
#[must_use]
pub fn policy_line(p: &ExecutionPolicy) -> String {
    format!(
        "durability={} workers={} evaluator={} conflict={:?} max_attempts={}",
        p.durability,
        p.workers,
        p.evaluator.name(),
        p.conflict,
        p.max_attempts
    )
}

impl std::fmt::Display for DemoReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "plan      branching — {} arms, fork / evaluate / select / commit",
            self.report.metrics.futures_forked
        )?;
        writeln!(f, "policy    {}", self.policy)?;
        writeln!(f)?;
        if let Some(s) = &self.report.selection {
            let scores: Vec<String> = s
                .candidates
                .iter()
                .zip(s.scores.iter())
                .map(|(f, v)| match v {
                    Some(n) => format!("future {f} = {n}"),
                    None => format!("future {f} = refused"),
                })
                .collect();
            writeln!(f, "selection {} won on `{}`", s.winner, s.rationale)?;
            writeln!(f, "          {}", scores.join("   "))?;
        }
        writeln!(f)?;
        writeln!(f, "{}", summary(&self.report, &self.policy))?;
        writeln!(f, "{}", effects(&self.sink))
    }
}

/// The core numbers, in the order a reader wants them.
pub fn summary(report: &RunReport, policy: &str) -> String {
    let m = &report.metrics;
    let mut out = String::new();
    let _ = writeln!(out, "trunk      {}", render(&report.trunk));
    let _ = writeln!(
        out,
        "authoritative future {} at version {}",
        report.authoritative, report.trunk_version
    );
    let _ = writeln!(
        out,
        "futures    {} forked, {} rejected, {} committed",
        m.futures_forked, m.futures_rejected, m.commits
    );
    let _ = writeln!(
        out,
        "latency    wall {}   executor work {}   parallelism {:.2}x",
        ms(m.wall_time),
        ms(m.executor_time),
        ratio(m.executor_time, m.wall_time)
    );
    let _ = writeln!(
        out,
        "durability {} records, {} group commits, {} bytes, peak speculation {}",
        m.records_durable, m.flushes, m.ledger_bytes, m.peak_speculation
    );
    let _ = writeln!(
        out,
        "waiting    {} synchronous waits totalling {}",
        m.sync_waits,
        ms(m.sync_wait)
    );
    let _ = writeln!(
        out,
        "storage    {} distinct state versions, {} deduplicated",
        m.state_versions, m.state_versions_deduped
    );
    let _ = writeln!(
        out,
        "recovery   {} attempt(s), {} step(s) discarded",
        m.recoveries, m.steps_discarded
    );
    let _ = writeln!(
        out,
        "conflicts  {} commit(s) needed a conflict policy",
        m.commits_with_conflicts
    );
    let _ = writeln!(out, "policy     {policy}");
    out
}

/// What reached the outside world.
pub fn effects(sink: &RecordingSink) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "effects    {} performed", sink.observed_len());
    for (key, op) in sink.observed() {
        let _ = writeln!(out, "             {key} -> {op}");
    }
    if sink.observed_len() == 0 {
        let _ = writeln!(out, "             (none: no effect escaped speculation)");
    }
    out
}

/// The canonical demonstration: fork, evaluate, commit, then recover and show the
/// graph is the same.
pub fn demo(arms: usize, flush_ms: u64, explore: bool) -> Result<String, String> {
    let plan = super::args::branching_plan(arms);
    let policy = if explore {
        ExecutionPolicy::exploring(Box::new(crate::HighestScore::new("score")), 8)
            .with_workers(arms.max(1))
    } else {
        ExecutionPolicy::durable()
    };
    let policy_line = policy_line(&policy);
    let cost = CostModel::with_flush_latency(flush_ms);
    let store = crate::MemoryStore::new();
    let sink = RecordingSink::new();
    let mut kernel = Kernel::with_ports(
        plan.clone(),
        policy,
        Box::new(store),
        Box::new(sink.clone()),
    )
    .map_err(|e| e.to_string())?
    .with_cost(cost);
    let report = kernel.run().map_err(|e| e.to_string())?;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "── the execution ────────────────────────────────────────────────"
    );
    out.push_str(
        &DemoReport {
            report: report.clone(),
            policy: policy_line.clone(),
            sink: sink.clone(),
            cost,
        }
        .to_string(),
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "── the lineage ─────────────────────────────────────────────────"
    );
    for (id, f) in kernel.lineage().iter() {
        let _ = writeln!(
            out,
            "future {id}  {:<18} base {}  head {}  {} step(s)",
            f.status.to_string(),
            f.base,
            f.head,
            f.steps()
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "── recovered from the durable ledger ────────────────────────────"
    );
    let records = kernel.records();
    let graph = crate::kernel::recovery::rebuild(records, crate::ExecutionId::new(1), plan.id)
        .map_err(|e| e.to_string())?;
    out.push_str(&graph_lines(&graph)?);
    let same = graph.trunk_state().logical_eq(&report.trunk);
    let _ = writeln!(
        out,
        "\nrecovered trunk == committed trunk: {}",
        if same { "yes" } else { "NO — this is a bug" }
    );
    Ok(out.trim_end().to_owned())
}

/// The same demonstration with the durability policy chosen explicitly.
pub fn explore(
    arms: usize,
    durability: &str,
    flush_ms: u64,
    workers: usize,
) -> Result<String, String> {
    let plan = super::args::branching_plan(arms);
    let policy = ExecutionPolicy::exploring(Box::new(crate::HighestScore::new("score")), 8)
        .with_durability(parse_durability(durability)?)
        .with_workers(workers);
    let policy_line = policy_line(&policy);
    let cost = CostModel::with_flush_latency(flush_ms);
    let sink = RecordingSink::new();
    let mut kernel = Kernel::with_ports(
        plan,
        policy,
        Box::new(crate::MemoryStore::new()),
        Box::new(sink.clone()),
    )
    .map_err(|e| e.to_string())?
    .with_cost(cost);
    let report = kernel.run().map_err(|e| e.to_string())?;
    Ok(DemoReport {
        report,
        policy: policy_line,
        sink,
        cost,
    }
    .to_string())
}

/// Parses the `DURABILITY` argument.
pub fn parse_durability(s: &str) -> Result<Durability, String> {
    let (name, arg) = s.split_once(':').unwrap_or((s, ""));
    let n = || -> Result<usize, String> {
        arg.parse::<usize>()
            .map_err(|e| format!("`{s}` needs a number after the colon: {e}"))
    };
    match name {
        "sync" | "synchronous" => Ok(Durability::Synchronous),
        "group-commit" | "group" => Ok(Durability::GroupCommit { batch: n()? }),
        "async" | "speculative" => Ok(Durability::Async { window: n()? }),
        other => Err(format!(
            "unknown durability `{other}` (expected sync, group-commit:N or async:W)"
        )),
    }
}

/// Which effect classes escape speculation, and which do not.
pub fn effects_demo(flush_ms: u64) -> Result<String, String> {
    let plan = super::args::effect_plan();
    let policy =
        ExecutionPolicy::exploring(Box::new(crate::HighestScore::new("score")), 8).with_workers(2);
    let policy = policy;
    let sink = RecordingSink::new();
    let mut kernel = Kernel::with_ports(
        plan,
        policy,
        Box::new(crate::MemoryStore::new()),
        Box::new(sink.clone()),
    )
    .map_err(|e| e.to_string())?
    .with_cost(CostModel::with_flush_latency(flush_ms));
    let report = kernel.run().map_err(|e| e.to_string())?;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "── effect classes under speculative execution ───────────────────"
    );
    let _ = writeln!(
        out,
        "{:<20} {:<10} {:<12} why",
        "class", "speculatable", "performed now"
    );
    for c in crate::domain::effect::EffectClass::ALL {
        let performed = sink.observed_len();
        let _ = writeln!(
            out,
            "{:<20} {:<10} {:<12} {}",
            c.as_str(),
            if c.speculatable() { "yes" } else { "no" },
            format!("{performed}"),
            why(c)
        );
    }
    let _ = writeln!(out);
    out.push_str(&effects(&sink));
    let _ = writeln!(
        out,
        "\nOnly the effects of the arm that was *committed* reached the world."
    );
    let _ = writeln!(out, "trunk: {}", render(&report.trunk));
    Ok(out.trim_end().to_owned())
}

fn why(c: crate::domain::effect::EffectClass) -> &'static str {
    use crate::domain::effect::EffectClass as C;
    match c {
        C::Pure => "no interaction at all",
        C::Read => "observation is journalled, so replay returns it",
        C::IdempotentWrite => "the target deduplicates by the runtime's key",
        C::Compensatable => "deferred until commit; a compensation is declared",
        C::Irreversible => "deferred until commit; nothing undoes it",
    }
}

/// A recovered graph, rendered.
pub fn graph(
    graph: &ExecutionGraph,
    durable_records: usize,
    torn_bytes: u64,
) -> Result<String, String> {
    let mut out = String::new();
    let _ = writeln!(out, "{}", graph_lines(graph)?);
    let _ = writeln!(
        out,
        "\n{} durable record(s), {torn_bytes} torn byte(s) dropped",
        durable_records
    );
    graph
        .validate()
        .map_err(|e| format!("the recovered graph is inconsistent: {e}"))?;
    let _ = writeln!(out, "graph validates: yes");
    Ok(out.trim_end().to_owned())
}

fn graph_lines(graph: &ExecutionGraph) -> Result<String, String> {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} record(s) replayed, trunk at version {}",
        graph.records, graph.trunk_version
    );
    for (id, f) in graph.lineage.iter() {
        let parent = f.parent.map_or_else(|| "-".to_owned(), |p| p.to_string());
        let _ = writeln!(
            out,
            "future {id:<4} {:<12} parent {parent:<6} base {}  head {}  {} step(s)",
            status(f.status),
            f.base,
            f.head,
            f.steps()
        );
    }
    if !graph.owed.is_empty() {
        let pending: Vec<String> = graph.owed.iter().map(|(f, o)| format!("{f}.{o}")).collect();
        let _ = writeln!(
            out,
            "owed: a durable commit still has {} effect(s) to release: {}",
            graph.owed.len(),
            pending.join(", ")
        );
    }
    if graph.authoritative != TRUNK {
        let _ = writeln!(out, "authoritative future: {}", graph.authoritative);
    }
    Ok(out.trim_end().to_owned())
}

fn status(s: FutureStatus) -> &'static str {
    match s {
        FutureStatus::Evaluable => "Evaluable",
        other => other.as_str(),
    }
}

fn render(state: &crate::State) -> String {
    state
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

fn ratio(a: Duration, b: Duration) -> f64 {
    if b.is_zero() {
        0.0
    } else {
        a.as_secs_f64() / b.as_secs_f64()
    }
}
