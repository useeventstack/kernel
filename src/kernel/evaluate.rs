//! Evaluation: turning a set of futures into one decision.
//!
//! # Division of responsibility
//!
//! The runtime knows **how** to execute a selection and commit: it knows how to
//! record the decision durably, how to settle the losers, how to release the
//! winner's deferred effects, and how to recover each of those. It does not know
//! **why** one future is better than another, and it is not allowed to guess.
//!
//! That is why the abstraction is so thin. An [`Evaluator`] maps a read-only
//! [`FutureView`] to an optional score, and the runtime takes the argmax with a
//! deterministic tie-break. A deterministic predicate, a cost model, a policy
//! constraint and an external or model-based evaluator are all just different
//! `Evaluator` implementations, and the runtime does not require any of them — in
//! particular it does not require a language model.
//!
//! # Determinism
//!
//! Selection is a *decision record*, so it has to be reproducible: the same set
//! of futures must always produce the same winner, or recovery would rebuild a
//! different graph than the one that was running. Two rules guarantee that:
//!
//! * scores are a function of the future's own head state and its accounting, so
//!   they do not depend on iteration order or on the wall clock;
//! * ties are broken by ascending [`FutureId`], never by "whoever was inserted
//!   first" or "whoever was fastest".

use std::fmt;

use crate::domain::future::FutureStatus;
use crate::domain::future::FutureView;
use crate::domain::ids::FutureId;

/// A score for one future, with the reason it was produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Score {
    pub value: i64,
    /// The evaluator's label for *why*, for reports. The runtime never reads it.
    pub because: &'static str,
}

impl Score {
    #[must_use]
    pub fn new(value: i64, because: &'static str) -> Self {
        Self { value, because }
    }
}

/// Decides whether a future is viable, and how good it is.
///
/// Not `Clone`: a policy owns its evaluator, and a policy is built once and then
/// belongs to one kernel. Sharing a policy across kernels would mean sharing a
/// scorer, and a scorer that is secretly shared is a scorer whose verdicts depend
/// on what else is running.
pub trait Evaluator: fmt::Debug + Send + Sync {
    /// A stable name, recorded in reports so a decision can be attributed.
    fn name(&self) -> &'static str;

    /// Scores `view`, or returns `None` if the future is not viable at all.
    ///
    /// Returning `None` is how a future is excluded: a policy constraint is just
    /// an evaluator that refuses. That is why evaluation and admissibility are
    /// the same mechanism, and why the runtime needs no separate "is this
    /// allowed" step that could disagree with the scoring.
    fn score(&self, view: &FutureView) -> Option<Score>;
}

/// Picks the future with the highest value of one state key.
///
/// The default, because it is the one a demo needs and the one a cost or quality
/// objective usually reduces to: write a number into a state key and pick the
/// largest.
#[derive(Debug, Clone)]
pub struct HighestScore {
    key: String,
}

impl HighestScore {
    #[must_use]
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_owned(),
        }
    }
}

impl Evaluator for HighestScore {
    fn name(&self) -> &'static str {
        "highest_score"
    }

    fn score(&self, view: &FutureView) -> Option<Score> {
        Some(Score::new(
            view.get(&self.key),
            "highest value of the scored key",
        ))
    }
}

/// Picks the future with the *lowest* value of one state key. For costs, latencies
/// and penalties, where less is better.
#[derive(Debug, Clone)]
pub struct LowestScore {
    key: String,
}

impl LowestScore {
    #[must_use]
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_owned(),
        }
    }
}

impl Evaluator for LowestScore {
    fn name(&self) -> &'static str {
        "lowest_score"
    }

    fn score(&self, view: &FutureView) -> Option<Score> {
        // Negate so the shared argmax picks the smallest value. The comparison
        // stays a single `max`, so there is exactly one place where selection can
        // go wrong.
        Some(Score::new(
            view.get(&self.key).saturating_neg(),
            "lowest value of the scored key",
        ))
    }
}

/// Picks the cheapest future, using the simulated time it consumed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cheapest;

impl Evaluator for Cheapest {
    fn name(&self) -> &'static str {
        "cheapest"
    }

    fn score(&self, view: &FutureView) -> Option<Score> {
        Some(Score::new(
            i64::try_from(view.cost_ms)
                .unwrap_or(i64::MAX)
                .saturating_neg(),
            "least simulated time",
        ))
    }
}

/// Accepts every viable future and lets the tie-break decide — effectively
/// "first by identity". Useful for testing that selection is deterministic and
/// that the *decision record* is what survives, not the ordering of evaluation.
#[derive(Debug, Clone, Copy, Default)]
pub struct FirstByIdentity;

impl Evaluator for FirstByIdentity {
    fn name(&self) -> &'static str {
        "first_by_identity"
    }

    fn score(&self, _view: &FutureView) -> Option<Score> {
        Some(Score::new(0, "identity order"))
    }
}

/// Accepts only futures that deferred at least one irreversible effect, then
/// takes the highest score.
///
/// A small but pointed example: an evaluator is the natural place for a
/// *constraint*, and a constraint that is expressed as "this plan is admissible"
/// is inseparable from the decision about which of the admissible plans wins.
#[derive(Debug, Clone)]
pub struct OnlyWithoutIrreversible {
    key: String,
}

impl OnlyWithoutIrreversible {
    #[must_use]
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_owned(),
        }
    }
}

impl Evaluator for OnlyWithoutIrreversible {
    fn name(&self) -> &'static str {
        "only_without_irreversible"
    }

    fn score(&self, view: &FutureView) -> Option<Score> {
        if view.irreversible_deferred > 0 {
            return None;
        }
        Some(Score::new(
            view.get(&self.key),
            "no irreversible effect deferred",
        ))
    }
}

/// The outcome of evaluating a set of futures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub candidates: Vec<FutureId>,
    /// Score per candidate, `None` for a future the evaluator refused.
    pub scores: Vec<Option<i64>>,
    pub winner: FutureId,
    /// Why the evaluator gave its score, for the report.
    pub rationale: &'static str,
}

/// No candidate was viable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoViableFuture {
    pub candidates: Vec<FutureId>,
}

impl fmt::Display for NoViableFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<String> = self.candidates.iter().map(ToString::to_string).collect();
        write!(
            f,
            "no viable future among [{}]: the evaluator refused all of them",
            names.join(", ")
        )
    }
}

impl std::error::Error for NoViableFuture {}

/// Scores every candidate and picks the winner.
///
/// The tie-break is ascending `FutureId`, which makes the result a function of
/// the *set* of candidates rather than of the order they were evaluated in. A
/// selection that depended on evaluation order would not be reproducible, and a
/// decision that is not reproducible cannot be recorded as a durable decision.
pub fn select(
    evaluator: &dyn Evaluator,
    views: &[FutureView],
) -> Result<Selection, NoViableFuture> {
    // Only a future that actually finished is a candidate. A future that failed,
    // was abandoned, or has not settled yet is not competing on equal terms: it
    // either never produced the state the evaluator reads, or produced a partial
    // one. Letting an evaluator score it would let a branch that did *less* work
    // win on a missing value, which is the opposite of what a speculative system
    // is for.
    let mut candidates: Vec<(FutureId, Option<i64>)> = views
        .iter()
        .filter(|v| v.status == FutureStatus::Evaluable)
        .map(|v| (v.id, evaluator.score(v).map(|s| s.value)))
        .collect();
    candidates.sort_by_key(|(id, _)| *id);

    let winner = candidates
        .iter()
        .filter_map(|(id, s)| s.map(|v| (*id, v)))
        .max_by_key(|(id, v)| (*v, std::cmp::Reverse(*id)))
        .map(|(id, _)| id);

    let Some(winner) = winner else {
        return Err(NoViableFuture {
            candidates: candidates.iter().map(|(id, _)| *id).collect(),
        });
    };
    let rationale = views
        .iter()
        .find(|v| v.id == winner)
        .and_then(|v| evaluator.score(v))
        .map_or("the evaluator gave no reason", |s| s.because);
    Ok(Selection {
        candidates: candidates.iter().map(|(id, _)| *id).collect(),
        scores: candidates.iter().map(|(_, s)| *s).collect(),
        winner,
        rationale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::future::FutureStatus;
    use crate::domain::state::State;

    fn view(id: u64, score: i64) -> FutureView {
        let mut state = State::new();
        state.set("score", score);
        FutureView {
            id: FutureId::new(id),
            parent: Some(FutureId::new(0)),
            arm: Some(0),
            status: FutureStatus::Evaluable,
            head: state.clone(),
            base: state,
            steps: 2,
            cost_ms: 10,
            deferred_effects: 0,
            performed_effects: 0,
            irreversible_deferred: 0,
            children: Vec::new(),
        }
    }

    #[test]
    fn the_highest_score_wins() {
        let views = [view(1, 10), view(2, 20), view(3, 15)];
        let s = select(&HighestScore::new("score"), &views).unwrap();
        assert_eq!(s.winner, FutureId::new(2));
        assert_eq!(s.scores, vec![Some(10), Some(20), Some(15)]);
    }

    #[test]
    fn selection_is_independent_of_the_order_candidates_are_supplied_in() {
        let a = select(&HighestScore::new("score"), &[view(1, 5), view(2, 9)]).unwrap();
        let b = select(&HighestScore::new("score"), &[view(2, 9), view(1, 5)]).unwrap();
        assert_eq!(a, b, "a durable decision must be reproducible");
    }

    #[test]
    fn ties_break_on_ascending_identity() {
        let s = select(&HighestScore::new("score"), &[view(7, 3), view(4, 3)]).unwrap();
        assert_eq!(s.winner, FutureId::new(4));
    }

    #[test]
    fn the_lowest_score_wins_for_costs() {
        let views = [view(1, 500), view(2, 5), view(3, 50)];
        let s = select(&LowestScore::new("score"), &views).unwrap();
        assert_eq!(s.winner, FutureId::new(2));
    }

    #[test]
    fn the_cheapest_evaluator_picks_the_least_cost() {
        let mut a = view(1, 0);
        a.cost_ms = 90;
        let mut b = view(2, 0);
        b.cost_ms = 3;
        let s = select(&Cheapest, &[a, b]).unwrap();
        assert_eq!(s.winner, FutureId::new(2));
    }

    #[test]
    fn a_refused_candidate_cannot_win() {
        let mut safe = view(1, 1);
        safe.irreversible_deferred = 0;
        let mut risky = view(2, 1000);
        risky.irreversible_deferred = 1;
        let s = select(&OnlyWithoutIrreversible::new("score"), &[safe, risky]).unwrap();
        assert_eq!(s.winner, FutureId::new(1));
        assert_eq!(s.scores, vec![Some(1), None]);
    }

    #[test]
    fn no_viable_candidate_is_an_error_naming_them_all() {
        let mut a = view(1, 1);
        a.irreversible_deferred = 1;
        let mut b = view(2, 2);
        b.irreversible_deferred = 1;
        let err = select(&OnlyWithoutIrreversible::new("score"), &[a, b]).unwrap_err();
        assert_eq!(err.candidates, vec![FutureId::new(1), FutureId::new(2)]);
        assert!(err.to_string().contains("no viable future"));
    }

    #[test]
    fn a_view_reports_its_own_write_set() {
        let mut v = view(1, 5);
        assert!(v.wrote().is_empty(), "base and head agree");
        v.base.set("score", 0);
        assert_eq!(v.wrote(), vec![("score".to_owned(), 0, 5)]);
    }

    #[test]
    fn the_rationale_is_reported_for_the_winner() {
        let views = [view(1, 1), view(2, 2)];
        let s = select(&HighestScore::new("score"), &views).unwrap();
        assert_eq!(s.rationale, "highest value of the scored key");
    }
}
