//! The effect model: what an execution may do to the outside world, and when.
//!
//! # The problem this solves
//!
//! Speculation is only safe when the work being speculated cannot escape. If a
//! future that is going to be *rejected* can send an email, then "evaluate three
//! futures and pick one" is not a decision procedure, it is a coin flip with
//! three emails.
//!
//! A durable log solves the mirror-image problem — it keeps execution and
//! durability in step — and says nothing at all about the world. So the runtime
//! needs its own answer, and the answer has to be a *rule*, not a convention.
//!
//! # The classification
//!
//! Every effect is in exactly one class, and the class answers one question:
//! **what happens to the outside world if this future is abandoned?**
//!
//! | class | can it be performed while the future is speculative? | on abandon |
//! |---|---|---|
//! | [`EffectClass::Pure`] | yes | nothing happened |
//! | [`EffectClass::Read`] | yes | nothing happened; the result is journalled so replay returns it |
//! | [`EffectClass::IdempotentWrite`] | yes, because the target deduplicates by key | a later commit is a no-op at the target |
//! | [`EffectClass::Compensatable`] | **no** — deferred to commit | nothing happened; a compensation exists if it did |
//! | [`EffectClass::Irreversible`] | **no** — deferred to commit | nothing happened |
//!
//! Five classes, and each is a distinct rule rather than a shade of the one
//! before it. `Read` and `Pure` differ because reads have to be recorded for
//! replay. `IdempotentWrite` differs from `Compensatable` because the safety
//! comes from the *target* honouring a key rather than from an undo action, and
//! because an idempotent write that *is* issued speculatively is cheaper than
//! deferring it. `Compensatable` differs from `Irreversible` because a
//! compensation is a real, if imperfect, undo, and a production runtime needs
//! the distinction to decide whether a committed future may ever need a saga.
//!
//! Note what the table does **not** say: nothing here promises that a
//! compensation fully undoes an effect. It says the world is *unchanged* until
//! commit. That is the guarantee the runtime can actually make, and it is the
//! one the tests assert.

use std::collections::BTreeMap;
use std::fmt;

use crate::domain::ids::{EffectId, ExecutionId, FutureId};
use crate::domain::version::Version;

/// What kind of interaction with the outside world an effect is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectClass {
    /// No interaction. Produces a state transition only.
    Pure,
    /// Observes the outside world. The result is recorded, so replay returns the
    /// recorded value instead of observing again.
    Read,
    /// Mutates the outside world; the target deduplicates by
    /// [`EffectKey`], so performing it twice is indistinguishable from once.
    IdempotentWrite,
    /// Mutates the outside world and declares a compensation.
    Compensatable,
    /// Mutates the outside world with no undo.
    Irreversible,
}

impl EffectClass {
    /// Every class, for exhaustive tests and reports.
    pub const ALL: [EffectClass; 5] = [
        EffectClass::Pure,
        EffectClass::Read,
        EffectClass::IdempotentWrite,
        EffectClass::Compensatable,
        EffectClass::Irreversible,
    ];

    /// Whether the effect may be performed while its future is speculative.
    ///
    /// This is the whole safety argument in one predicate, and it is a total
    /// function of the class — there is no policy, no configuration and no
    /// override. A future cannot opt in to performing an irreversible effect
    /// early, because the API offers no way to ask.
    #[must_use]
    pub fn speculatable(self) -> bool {
        matches!(
            self,
            EffectClass::Pure | EffectClass::Read | EffectClass::IdempotentWrite
        )
    }

    /// Whether the effect mutates the world as opposed to observing it.
    #[must_use]
    pub fn is_write(self) -> bool {
        !matches!(self, EffectClass::Pure | EffectClass::Read)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EffectClass::Pure => "pure",
            EffectClass::Read => "read",
            EffectClass::IdempotentWrite => "idempotent_write",
            EffectClass::Compensatable => "compensatable",
            EffectClass::Irreversible => "irreversible",
        }
    }

    /// Wire code, used by the ledger encoding.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            EffectClass::Pure => 0,
            EffectClass::Read => 1,
            EffectClass::IdempotentWrite => 2,
            EffectClass::Compensatable => 3,
            EffectClass::Irreversible => 4,
        }
    }

    /// Inverse of [`EffectClass::code`].
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => EffectClass::Pure,
            1 => EffectClass::Read,
            2 => EffectClass::IdempotentWrite,
            3 => EffectClass::Compensatable,
            4 => EffectClass::Irreversible,
            _ => return None,
        })
    }
}

impl fmt::Display for EffectClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stable identity for one effect occurrence, used as the idempotency key.
///
/// Derived from *where in the plan* the effect sits, never supplied by the caller
/// and never taken from a runtime-assigned id. Two consequences follow, and both
/// matter:
///
/// * re-executing the same step after a crash produces the *same* key, so a retry
///   cannot double-apply an effect;
/// * two different futures asking for the same effect get *different* keys, so a
///   rejected branch's write can never be absorbed by the branch that wins.
///
/// The `FutureId` deliberately does **not** appear in the key. It is assigned by
/// the runtime and it is *not* stable under retry: a rollback discards the futures
/// a fork created, and re-executing the fork allocates fresh ids. A key built from
/// it would look stable right up until the first crash, at which point a
/// speculative `IdempotentWrite` would be re-issued under a fresh key and the
/// target, seeing a request it has never seen, would apply it a second time. The
/// idempotency token has to survive the thing it is deduplicating, so it is built
/// from the arm path — the sequence of fork choices from the root — which does.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectKey {
    /// The execution this effect belongs to, so two executions of the same plan
    /// never share a key against a shared target.
    pub execution: ExecutionId,
    /// Sequence of fork-arm indices from the root future. Empty for the trunk.
    pub path: Vec<u32>,
    pub ordinal: EffectId,
}

impl EffectKey {
    #[must_use]
    pub fn new(execution: ExecutionId, path: Vec<u32>, ordinal: EffectId) -> Self {
        Self {
            execution,
            path,
            ordinal,
        }
    }

    /// The fork-arm path this key belongs to.
    #[must_use]
    pub fn path(&self) -> &[u32] {
        &self.path
    }

    /// Compact, stable textual form used in reports and in `Sink` keys.
    #[must_use]
    pub fn as_string(&self) -> String {
        if self.path.is_empty() {
            format!(
                "e{}/trunk.e{}",
                self.execution.value(),
                self.ordinal.value()
            )
        } else {
            let arms = self
                .path
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(".");
            format!(
                "e{}/{}.e{}",
                self.execution.value(),
                arms,
                self.ordinal.value()
            )
        }
    }
}

impl fmt::Display for EffectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

/// The value an effect produces.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EffectValue {
    Int(i64),
    Text(String),
    /// The effect produced nothing observable.
    Unit,
}

impl EffectValue {
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            EffectValue::Int(v) => Some(*v),
            _ => None,
        }
    }
}

impl fmt::Display for EffectValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EffectValue::Int(v) => write!(f, "{v}"),
            EffectValue::Text(s) => write!(f, "{s:?}"),
            EffectValue::Unit => f.write_str("()"),
        }
    }
}

/// What an effect asks the world to do. The prototype keeps this a small,
/// fully deterministic description so that the sink can be a pure function and
/// the tests can assert on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectOp {
    /// Logical name, e.g. `charge_card` or `fetch_quote`.
    pub name: String,
    /// Deterministic payload the sink interprets.
    pub args: i64,
}

impl EffectOp {
    #[must_use]
    pub fn new(name: &str, args: i64) -> Self {
        Self {
            name: name.to_owned(),
            args,
        }
    }
}

impl fmt::Display for EffectOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.name, self.args)
    }
}

/// What the runtime actually did with a requested effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Performed now. The result is journalled.
    Performed { result: EffectValue },
    /// Not performed: the journal already holds the result of this exact key, so
    /// replay returns the recorded value instead of observing the world again.
    Replayed { result: EffectValue },
    /// Recorded as an intent and *not* visible to the outside world. It will be
    /// performed when, and only when, its future is committed.
    Deferred,
}

impl Disposition {
    #[must_use]
    pub fn result(&self) -> Option<&EffectValue> {
        match self {
            Disposition::Performed { result } | Disposition::Replayed { result } => Some(result),
            Disposition::Deferred => None,
        }
    }

    /// Whether the outside world was touched.
    #[must_use]
    pub fn touched_world(&self) -> bool {
        matches!(self, Disposition::Performed { .. })
    }

    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Disposition::Performed { .. } => "performed",
            Disposition::Replayed { .. } => "replayed",
            Disposition::Deferred => "deferred",
        }
    }
}

impl fmt::Display for Disposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a plan asked for, plus what the runtime did with it. This is the row
/// the effect journal stores, and it is the unit that makes replay
/// deterministic: the result of an observation is in here, not in the world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectRequest {
    pub key: EffectKey,
    /// The future that asked.
    pub future: FutureId,
    /// The future version at the time of the request.
    pub version: Version,
    pub class: EffectClass,
    pub op: EffectOp,
    /// State key the result is folded into, for `Read` effects.
    pub reads_into: Option<String>,
    /// State key the effect will write, if the plan declares one. Recorded for
    /// conflict reporting; the merge itself compares whole states.
    pub writes: Option<String>,
    /// A declared undo, for `Compensatable` effects.
    pub compensation: Option<EffectOp>,
}

impl EffectRequest {
    #[must_use]
    pub fn new(
        key: EffectKey,
        future: FutureId,
        version: Version,
        class: EffectClass,
        op: EffectOp,
    ) -> Self {
        Self {
            key,
            future,
            version,
            class,
            op,
            reads_into: None,
            writes: None,
            compensation: None,
        }
    }

    #[must_use]
    pub fn reading_into(mut self, key: &str) -> Self {
        self.reads_into = Some(key.to_owned());
        self
    }

    #[must_use]
    pub fn writing(mut self, key: &str) -> Self {
        self.writes = Some(key.to_owned());
        self
    }

    #[must_use]
    pub fn with_compensation(mut self, op: EffectOp) -> Self {
        self.compensation = Some(op);
        self
    }

    /// Whether the runtime is allowed to perform this effect now.
    ///
    /// `authoritative` is true for the trunk, and for a future whose commit
    /// record is durable. It is the only input: the rule does not depend on how
    /// far ahead of durability the future is running.
    #[must_use]
    pub fn may_perform_now(&self, authoritative: bool) -> bool {
        authoritative || self.class.speculatable()
    }
}

// ---------------------------------------------------------------------------
// The sink port
// ---------------------------------------------------------------------------

/// What happened when an effect met the world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectError {
    /// The target refused the operation. Retrying is meaningless.
    Refused { op: String, reason: String },
    /// The target is unavailable. Retrying may succeed.
    Unavailable { op: String },
}

impl fmt::Display for EffectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EffectError::Refused { op, reason } => write!(f, "effect `{op}` refused: {reason}"),
            EffectError::Unavailable { op } => write!(f, "effect `{op}` unavailable"),
        }
    }
}

impl std::error::Error for EffectError {}

/// The outside world, as far as the runtime is concerned.
///
/// This is a port, not an implementation. The runtime never learns what a
/// target is, only that it can be handed a key and an operation. The guarantee
/// the runtime relies on is *key-based deduplication*: performing the same
/// `EffectKey` twice is indistinguishable from performing it once. A target
/// that does not honour its keys gets at-least-once delivery and the runtime
/// says so — it does not silently claim exactly-once.
pub trait EffectSink {
    /// Performs `op` under `key`. If `key` was performed before, the recorded
    /// result is returned and the target is not touched again.
    fn perform(
        &mut self,
        key: &EffectKey,
        op: &EffectOp,
        class: EffectClass,
    ) -> Result<EffectValue, EffectError>;
}

/// The log a [`RecordingSink`] keeps.
#[derive(Debug, Default)]
struct SinkLog {
    /// Every call to `perform`, in order, including deduplicated ones.
    attempts: Vec<(EffectKey, EffectOp)>,
    /// The first result observed per key. This is what "the target honours the
    /// key" means operationally.
    applied: BTreeMap<EffectKey, EffectValue>,
    /// Every effect that actually reached the world, in order.
    observed: Vec<(EffectKey, EffectOp)>,
    /// Keys to fail, once each.
    failing: BTreeMap<EffectKey, EffectError>,
    /// How many times a deduplicated call was served from `applied`.
    dedup_hits: u64,
    /// Result for a read, as a pure function of the operation.
    read_bias: i64,
}

/// An in-memory, fully deterministic sink that *does* honour keys.
///
/// Two properties make it useful for testing claims that would otherwise need a
/// real payment processor:
///
/// * every `perform` is recorded, so a test can assert that an effect of a
///   rejected future was **never** issued, not merely never committed;
/// * keys are deduplicated, so a test can assert that a retried commit applies an
///   effect **exactly once**.
///
/// Clones share one log. That matters because the kernel *owns* its sink: a caller
/// hands one in and then wants to ask afterwards what the world actually saw. If a
/// clone had its own log, the honest answer would be "I can't tell", which is the
/// one answer that makes effect-safety untestable.
#[derive(Debug, Clone, Default)]
pub struct RecordingSink {
    log: std::rc::Rc<std::cell::RefCell<SinkLog>>,
}

impl RecordingSink {
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: std::rc::Rc::new(std::cell::RefCell::new(SinkLog {
                read_bias: 0,
                ..SinkLog::default()
            })),
        }
    }

    /// Sets the offset added to every `Read` result, so different branches can
    /// observe different worlds.
    #[must_use]
    pub fn with_read_bias(self, bias: i64) -> Self {
        self.log.borrow_mut().read_bias = bias;
        self
    }

    /// Makes the next `perform` of `key` fail. Used to test
    /// crash-between-decision-and-effect.
    pub fn fail_once(&self, key: EffectKey, error: EffectError) {
        self.log.borrow_mut().failing.insert(key, error);
    }

    /// Every effect that actually reached the world, in order.
    #[must_use]
    pub fn observed(&self) -> Vec<(EffectKey, EffectOp)> {
        self.log.borrow().observed.clone()
    }

    /// Every call to `perform`, including ones the target deduplicated.
    #[must_use]
    pub fn attempts(&self) -> Vec<(EffectKey, EffectOp)> {
        self.log.borrow().attempts.clone()
    }

    /// How many times a key was served from the applied map rather than by
    /// performing again.
    #[must_use]
    pub fn dedup_hits(&self) -> u64 {
        self.log.borrow().dedup_hits
    }

    #[must_use]
    pub fn observed_len(&self) -> usize {
        self.log.borrow().observed.len()
    }

    /// Whether any effect with this name ever reached the world.
    #[must_use]
    pub fn observed_name(&self, name: &str) -> bool {
        self.log
            .borrow()
            .observed
            .iter()
            .any(|(_, op)| op.name == name)
    }

    /// Forgets the record of observed effects while keeping the dedup table, so a
    /// test can tell "performed once in total" from "performed once in this phase".
    pub fn clear_observed(&self) {
        let mut log = self.log.borrow_mut();
        log.observed.clear();
        log.attempts.clear();
    }
}

impl EffectSink for RecordingSink {
    fn perform(
        &mut self,
        key: &EffectKey,
        op: &EffectOp,
        class: EffectClass,
    ) -> Result<EffectValue, EffectError> {
        let mut log = self.log.borrow_mut();
        log.attempts.push((key.clone(), op.clone()));
        if let Some(error) = log.failing.remove(key) {
            return Err(error);
        }
        if let Some(existing) = log.applied.get(key).cloned() {
            log.dedup_hits += 1;
            return Ok(existing);
        }
        let result = match class {
            EffectClass::Read => EffectValue::Int(op.args * 2 + log.read_bias),
            _ => EffectValue::Unit,
        };
        log.applied.insert(key.clone(), result.clone());
        log.observed.push((key.clone(), op.clone()));
        Ok(result)
    }
}

/// A sink that performs nothing, and records every attempt to.
///
/// # What "suppressed" means
///
/// A suppressed effect is **not** "the effect did not run". It is **not issued,
/// and later replayable** — the intent is still journalled by the runtime, the
/// operation is still described here, and a later authoritative execution can
/// still issue it. What did not happen is the *contact with the world*. That
/// distinction is the whole point of this type, and it is why the type is
/// public rather than a test-local stub: a sink that swallowed a write and a
/// sink that never issued one look identical from the outside, and the product
/// needs to be able to tell them apart.
///
/// # What a suppressed read returns
///
/// `EffectValue::Unit`, and no value. A sink that invented a number here would
/// be fabricating an observation, which is the one thing an effect port must
/// never do. This is not a limitation to work around: the runtime consults the
/// effect journal *before* the sink, so a replayed run returns the recorded
/// value without ever asking the world again. A run that suppresses a read
/// whose result is not already in the journal cannot recover that value, and
/// [`Self::suppressed_unreadable`] reports how many such reads there were so
/// the caller can be told rather than left with a plausible wrong number.
///
/// # Why there are two accessors
///
/// [`Self::attempted`] is everything the runtime tried to do, and
/// [`Self::issued`] is everything that reached the world. The second is always
/// empty. A test that only checked `issued().is_empty()` would pass against a
/// sink that also silently dropped real effects, so the pair is the assertion
/// and the emptiness is a consequence of the implementation.
#[derive(Debug, Clone, Default)]
pub struct SuppressedSink {
    attempts: std::rc::Rc<std::cell::RefCell<Vec<SuppressedAttempt>>>,
}

/// One effect the runtime tried to perform, and did not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuppressedAttempt {
    pub key: EffectKey,
    pub op: EffectOp,
    pub class: EffectClass,
}

impl SuppressedSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything the runtime asked the world to do, in order, and what it
    /// returned instead.
    #[must_use]
    pub fn attempted(&self) -> Vec<SuppressedAttempt> {
        self.attempts.borrow().clone()
    }

    /// Everything that reached the world. Structurally empty: this is what a
    /// `replay` or an `observe` run asserts on.
    #[must_use]
    pub fn issued(&self) -> Vec<(EffectKey, EffectOp)> {
        Vec::new()
    }

    #[must_use]
    pub fn attempted_len(&self) -> usize {
        self.attempts.borrow().len()
    }

    /// Whether an effect with this name was *offered* to the world, regardless
    /// of whether it was issued.
    #[must_use]
    pub fn attempted_name(&self, name: &str) -> bool {
        self.attempts.borrow().iter().any(|a| a.op.name == name)
    }

    /// How many suppressed `Read` effects had no journalled result to fall back
    /// on. A non-zero count means the run produced `EffectValue::Unit` where the
    /// real world would have produced a value.
    #[must_use]
    pub fn suppressed_unreadable(&self) -> usize {
        self.attempts
            .borrow()
            .iter()
            .filter(|a| a.class == EffectClass::Read)
            .count()
    }

    /// Forgets the attempts while keeping the "nothing was issued" guarantee,
    /// which is unaffected.
    pub fn clear_attempted(&self) {
        self.attempts.borrow_mut().clear();
    }
}

impl EffectSink for SuppressedSink {
    fn perform(
        &mut self,
        key: &EffectKey,
        op: &EffectOp,
        class: EffectClass,
    ) -> Result<EffectValue, EffectError> {
        // The runtime only calls `perform` when the effect *may* be performed now,
        // and it releases a deferred effect by calling `perform` again once the
        // commit record is durable. So an attempt recorded here is by
        // construction an effect the runtime was willing to issue to the world.
        self.attempts.borrow_mut().push(SuppressedAttempt {
            key: key.clone(),
            op: op.clone(),
            class,
        });
        Ok(EffectValue::Unit)
    }
}

/// A sink that performs reads and suppresses every write.
///
/// # Why this is not [`SuppressedSink`]
///
/// Suppressing a read and suppressing a write are **different operations**, and
/// conflating them produces a runtime that quietly computes a different answer
/// under a different strategy — which is the exact failure the strategy
/// agreement property exists to prevent.
///
/// A read has a *result the execution consumes*. Suppress it and the state a
/// plan folds into is not the state a real run produces; the run is no longer
/// the same run with fewer side effects, it is a different run. A write has no
/// such coupling: not doing it leaves the computation intact and only the world
/// unchanged.
///
/// So:
///
/// * **observe** — "evaluate, take no action of its own" — reads the world and
///   writes nothing. A read is not an action.
/// * **replay** — "rebuild from the log alone" — neither reads nor writes, and
///   gets its read results from the journal, which is consulted *before* the
///   sink. If a read was never journalled, [`SuppressedSink::suppressed_unreadable`]
///   counts it, because that is a hole the caller must be told about rather than
///   papered over with a fabricated value.
///
/// The distinction is a type, not a comment, so a strategy cannot choose the
/// wrong one by accident.
#[derive(Debug, Clone, Default)]
pub struct ObservationOnlySink {
    reads: RecordingSink,
    writes: std::rc::Rc<std::cell::RefCell<Vec<SuppressedAttempt>>>,
}

impl ObservationOnlySink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The reads that were performed, in order.
    #[must_use]
    pub fn observed(&self) -> Vec<(EffectKey, EffectOp)> {
        self.reads.observed()
    }

    /// The writes that were offered and refused, in order. Every one of them is a
    /// `Compensatable` or `Irreversible` (or an `IdempotentWrite`) the runtime was
    /// willing to perform and this sink did not.
    #[must_use]
    pub fn suppressed(&self) -> Vec<SuppressedAttempt> {
        self.writes.borrow().clone()
    }

    #[must_use]
    pub fn suppressed_len(&self) -> usize {
        self.writes.borrow().len()
    }

    /// Everything that reached the world, which is exactly the reads.
    #[must_use]
    pub fn issued(&self) -> Vec<(EffectKey, EffectOp)> {
        self.reads.observed()
    }
}

impl EffectSink for ObservationOnlySink {
    fn perform(
        &mut self,
        key: &EffectKey,
        op: &EffectOp,
        class: EffectClass,
    ) -> Result<EffectValue, EffectError> {
        if class == EffectClass::Read {
            return self.reads.perform(key, op, class);
        }
        self.writes.borrow_mut().push(SuppressedAttempt {
            key: key.clone(),
            op: op.clone(),
            class,
        });
        Ok(EffectValue::Unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::EffectId;

    fn key(n: u64) -> EffectKey {
        EffectKey::new(ExecutionId::new(1), vec![n as u32], EffectId::new(0))
    }

    #[test]
    fn only_pure_read_and_idempotent_effects_are_speculatable() {
        assert!(EffectClass::Pure.speculatable());
        assert!(EffectClass::Read.speculatable());
        assert!(EffectClass::IdempotentWrite.speculatable());
        assert!(!EffectClass::Compensatable.speculatable());
        assert!(!EffectClass::Irreversible.speculatable());
    }

    #[test]
    fn a_speculative_future_may_not_perform_a_write_it_cannot_undo() {
        let req = EffectRequest::new(
            key(1),
            FutureId::new(1),
            Version::new(3),
            EffectClass::Irreversible,
            EffectOp::new("charge_card", 100),
        );
        assert!(!req.may_perform_now(false), "speculative");
        assert!(req.may_perform_now(true), "authoritative");
    }

    #[test]
    fn a_speculative_future_may_perform_an_idempotent_write() {
        let req = EffectRequest::new(
            key(1),
            FutureId::new(1),
            Version::new(3),
            EffectClass::IdempotentWrite,
            EffectOp::new("upsert", 1),
        );
        assert!(req.may_perform_now(false));
    }

    #[test]
    fn a_key_survives_a_retry_that_allocates_a_new_future_id() {
        // The property the whole at-least-once story rests on: re-running the same
        // arm after a rollback must produce the same token, even though the runtime
        // hands the re-run a brand new FutureId.
        let first = EffectKey::new(ExecutionId::new(7), vec![2, 0], EffectId::new(3));
        let after_rollback = EffectKey::new(ExecutionId::new(7), vec![2, 0], EffectId::new(3));
        assert_eq!(first, after_rollback);
        assert_eq!(first.as_string(), "e7/2.0.e3");
        // And a different arm, execution, or ordinal must not collide with it.
        assert_ne!(
            first,
            EffectKey::new(ExecutionId::new(7), vec![2, 1], EffectId::new(3))
        );
        assert_ne!(
            first,
            EffectKey::new(ExecutionId::new(8), vec![2, 0], EffectId::new(3))
        );
        assert_ne!(
            first,
            EffectKey::new(ExecutionId::new(7), vec![2, 0], EffectId::new(4))
        );
    }

    #[test]
    fn keys_distinguish_arms_and_are_stable_per_arm() {
        assert_ne!(key(1), key(2));
        let a = EffectKey::new(ExecutionId::new(1), vec![1], EffectId::new(4));
        let b = EffectKey::new(ExecutionId::new(1), vec![1], EffectId::new(4));
        assert_eq!(a, b);
        assert_eq!(a.as_string(), "e1/1.e4");
        // The trunk is the empty path and is its own identity.
        let trunk = EffectKey::new(ExecutionId::new(1), Vec::new(), EffectId::new(4));
        assert_eq!(trunk.as_string(), "e1/trunk.e4");
        assert_ne!(trunk, a);
    }

    #[test]
    fn sink_honours_keys_so_a_repeat_performs_once() {
        let mut sink = RecordingSink::new();
        let op = EffectOp::new("charge", 5);
        sink.perform(&key(1), &op, EffectClass::IdempotentWrite)
            .unwrap();
        sink.perform(&key(1), &op, EffectClass::IdempotentWrite)
            .unwrap();
        assert_eq!(sink.observed_len(), 1, "the world was touched once");
        assert_eq!(sink.dedup_hits(), 1);
        assert_eq!(
            sink.attempts().len(),
            2,
            "both calls were made; the target deduped"
        );
    }

    #[test]
    fn sink_records_reads_with_a_bias_so_branches_can_differ() {
        let op = EffectOp::new("quote", 10);
        let a = RecordingSink::new().with_read_bias(0);
        let b = RecordingSink::new().with_read_bias(100);
        let mut sa = a;
        let mut sb = b;
        assert_eq!(
            sa.perform(&key(1), &op, EffectClass::Read).unwrap(),
            EffectValue::Int(20)
        );
        assert_eq!(
            sb.perform(&key(1), &op, EffectClass::Read).unwrap(),
            EffectValue::Int(120)
        );
    }

    #[test]
    fn class_codes_round_trip() {
        for c in EffectClass::ALL {
            assert_eq!(EffectClass::from_code(c.code()), Some(c));
        }
        assert_eq!(EffectClass::from_code(200), None);
    }

    #[test]
    fn writes_are_exactly_compensatable_and_irreversible() {
        let writes: Vec<_> = EffectClass::ALL
            .into_iter()
            .filter(|c| c.is_write())
            .collect();
        assert_eq!(
            writes,
            vec![
                EffectClass::IdempotentWrite,
                EffectClass::Compensatable,
                EffectClass::Irreversible
            ]
        );
    }

    #[test]
    fn a_suppressed_sink_issues_nothing_but_records_every_attempt() {
        let sink = SuppressedSink::new();
        let mut s = sink.clone();
        for class in EffectClass::ALL {
            s.perform(&key(1), &EffectOp::new("charge", 5), class)
                .unwrap();
        }
        assert!(sink.issued().is_empty(), "nothing reached the world");
        assert_eq!(sink.attempted_len(), 5, "but every attempt is recorded");
        assert!(sink.attempted_name("charge"));
        let classes: Vec<_> = sink.attempted().iter().map(|a| a.class).collect();
        assert_eq!(classes, EffectClass::ALL.to_vec());
    }

    #[test]
    fn a_suppressed_read_reports_no_value_rather_than_inventing_one() {
        let mut sink = SuppressedSink::new();
        let out = sink
            .perform(&key(1), &EffectOp::new("quote", 10), EffectClass::Read)
            .unwrap();
        assert_eq!(out, EffectValue::Unit);
        assert_eq!(
            sink.suppressed_unreadable(),
            1,
            "the caller must be able to tell that a real value was unavailable"
        );
    }

    #[test]
    fn an_observation_only_sink_reads_and_writes_nothing() {
        let sink = ObservationOnlySink::new();
        let mut s = sink.clone();
        let read = s
            .perform(&key(1), &EffectOp::new("quote", 10), EffectClass::Read)
            .unwrap();
        assert_eq!(
            read,
            EffectValue::Int(20),
            "a read must return the real value, or the run is a different run"
        );
        s.perform(
            &key(2),
            &EffectOp::new("charge", 5),
            EffectClass::Irreversible,
        )
        .unwrap();

        assert_eq!(sink.issued(), sink.observed());
        assert_eq!(
            sink.suppressed_len(),
            1,
            "the write was refused, and the refusal is on the record"
        );
        let suppressed = sink.suppressed();
        assert_eq!(suppressed[0].class, EffectClass::Irreversible);
        assert!(suppressed.iter().all(|a| a.class.is_write()));
    }

    #[test]
    fn the_two_suppressing_sinks_disagree_about_a_read_and_that_is_the_point() {
        let op = EffectOp::new("quote", 10);
        let mut all = SuppressedSink::new();
        let mut reads = ObservationOnlySink::new();
        assert_eq!(
            all.perform(&key(1), &op, EffectClass::Read).unwrap(),
            EffectValue::Unit
        );
        assert_eq!(
            reads.perform(&key(1), &op, EffectClass::Read).unwrap(),
            EffectValue::Int(20)
        );
    }

    #[test]
    fn suppressing_one_effect_is_indistinguishable_from_never_asking() {
        // The distinction that the type exists to preserve: the two look the same
        // to the world, and the difference is only visible afterwards.
        let mut silent = SuppressedSink::new();
        let _ = silent.perform(
            &key(1),
            &EffectOp::new("charge", 5),
            EffectClass::Irreversible,
        );
        assert!(silent.issued().is_empty());
        assert_eq!(
            silent.attempted_len(),
            1,
            "the intent is still on the record"
        );
        silent.clear_attempted();
        assert!(silent.issued().is_empty());
        assert_eq!(
            silent.attempted_len(),
            0,
            "a cleared sink is not a lying one"
        );
    }
}
