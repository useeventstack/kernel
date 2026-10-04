//! The effect journal: what every future has observed and requested.
//!
//! # Why this exists at all
//!
//! Replay is only deterministic if values that came from outside come back from
//! somewhere. A workflow that reads a clock, a random source or a remote service
//! produces a different answer on every run, so a runtime that replays cannot
//! reproduce the state it is replaying *from*.
//!
//! The usual answers each cost something:
//!
//! * **require the workflow to be deterministic** (Temporal, Restate, Vercel) —
//!   correct, but it makes observing the world impossible, so every interesting
//!   workflow has to be restructured around an activity;
//! * **do not replay, repair instead** (libDSE) — correct, but two futures computed
//!   from different observations cannot be compared, and a replay cannot be told
//!   apart from a first execution.
//!
//! The mechanism here is the narrowest of the three: **nondeterminism enters
//! through explicit, recordable boundaries**. A step cannot observe the world
//! except by requesting an [`crate::domain::EffectClass::Read`], the result is
//! written here, and a re-execution consults this index instead of the world. So:
//!
//! * the value survives a rollback, because the entry is keyed by
//!   `(future, ordinal)`;
//! * the key does not depend on the attempt, so a retry after a crash is the same
//!   request rather than a new one;
//! * and this is *structural* — there is no code path that reads the world without
//!   journalling it, so the guarantee is not a rule a user can forget.

use std::collections::BTreeMap;

use crate::domain::effect::{Disposition, EffectClass, EffectKey, EffectOp, EffectValue};
use crate::domain::ids::FutureId;

/// One journalled effect, from the point of view of replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalEntry {
    pub key: EffectKey,
    /// The runtime future that asked for it.
    ///
    /// Kept beside the key rather than derived from it, because the key is built
    /// from the stable arm path precisely so that it does *not* carry the
    /// volatile future id. Reclaiming a terminal future's journal entries needs
    /// both.
    pub future: FutureId,
    pub class: EffectClass,
    pub op: EffectOp,
    /// The value the world returned the first time, if it was observed.
    pub result: Option<EffectValue>,
    /// Whether the effect is still an unissued intent.
    pub deferred: bool,
}

impl JournalEntry {
    /// What replay should do with this entry.
    ///
    /// The rule is one line and it is the whole of the determinism guarantee:
    /// **a journal entry that holds an observed value is returned, never
    /// recomputed.**
    #[must_use]
    pub fn replay(&self) -> Disposition {
        match &self.result {
            Some(value) => Disposition::Replayed {
                result: value.clone(),
            },
            None => Disposition::Deferred,
        }
    }
}

/// Every effect every future has requested, indexed by its idempotency key.
///
/// This is a *derived* index, not a second source of truth: it is rebuilt from the
/// ledger on recovery, and the ledger alone is sufficient to rebuild it. That
/// distinction is why there is no consistency argument between the journal and the
/// log to make.
#[derive(Clone, Debug, Default)]
pub struct EffectJournal {
    entries: BTreeMap<EffectKey, JournalEntry>,
}

impl EffectJournal {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an observation, so a later re-execution returns it.
    pub fn record_performed(
        &mut self,
        key: EffectKey,
        future: FutureId,
        op: EffectOp,
        class: EffectClass,
        result: EffectValue,
    ) {
        self.entries.insert(
            key.clone(),
            JournalEntry {
                key,
                future,
                class,
                op,
                result: Some(result),
                deferred: false,
            },
        );
    }

    /// Records an intent that has not been issued.
    pub fn record_intent(
        &mut self,
        key: EffectKey,
        future: FutureId,
        op: EffectOp,
        class: EffectClass,
    ) {
        self.entries.entry(key.clone()).or_insert(JournalEntry {
            key,
            future,
            class,
            op,
            result: None,
            deferred: true,
        });
    }

    /// Marks an intent as issued, storing the result the world gave.
    pub fn mark_issued(&mut self, key: &EffectKey, result: EffectValue) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.deferred = false;
            entry.result = Some(result);
        }
    }

    #[must_use]
    pub fn get(&self, key: &EffectKey) -> Option<&JournalEntry> {
        self.entries.get(key)
    }

    /// The recorded result for `key`, if this effect was ever observed.
    #[must_use]
    pub fn recorded(&self, key: &EffectKey) -> Option<&EffectValue> {
        self.entries.get(key).and_then(|e| e.result.as_ref())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&EffectKey, &JournalEntry)> {
        self.entries.iter()
    }

    /// Keys belonging to one future, in ordinal order.
    #[must_use]
    pub fn for_future(&self, future: FutureId) -> Vec<&JournalEntry> {
        let mut out: Vec<&JournalEntry> = self
            .entries
            .values()
            .filter(|e| e.future == future)
            .collect();
        out.sort_by_key(|e| e.key.ordinal);
        out
    }

    /// Intents for one future that have not been issued yet, in ordinal order.
    ///
    /// This is the list a commit releases, and it is why a rejected future's
    /// irreversible effects provably never happen: the effect was recorded as an
    /// intent and the future was settled `Rejected` without ever being committed,
    /// so nothing ever reads this list for it.
    #[must_use]
    pub fn pending_release(&self, future: FutureId) -> Vec<&JournalEntry> {
        self.for_future(future)
            .into_iter()
            .filter(|e| e.deferred && e.class.is_write())
            .collect()
    }

    /// Keys nobody may need any more: the journal of a terminal future, once its
    /// commit is durable and its effects are released.
    ///
    /// Reclamation is deliberately conservative. An entry is dropped only when its
    /// future is terminal *and* no unissued write remains, because dropping an
    /// entry for a future that might be rolled back and re-executed would
    /// reintroduce exactly the nondeterminism the journal exists to prevent.
    #[must_use]
    pub fn reclaimable(&self, terminal: &[FutureId]) -> Vec<EffectKey> {
        self.entries
            .values()
            .filter(|e| terminal.contains(&e.future) && !e.deferred)
            .map(|e| e.key.clone())
            .collect()
    }

    /// Forgets the given keys. Only callable with keys from
    /// [`EffectJournal::reclaimable`].
    pub fn forget(&mut self, keys: &[EffectKey]) {
        for k in keys {
            self.entries.remove(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{EffectId, ExecutionId, FutureId};

    fn key(f: u64, o: u64) -> EffectKey {
        EffectKey::new(ExecutionId::new(1), vec![f as u32], EffectId::new(o))
    }

    fn op() -> EffectOp {
        EffectOp::new("quote", 10)
    }

    #[test]
    fn a_recorded_read_is_replayed_not_recomputed() {
        let mut j = EffectJournal::new();
        j.record_performed(
            key(1, 0),
            FutureId::new(1),
            op(),
            EffectClass::Read,
            EffectValue::Int(99),
        );
        let entry = j.get(&key(1, 0)).unwrap();
        assert_eq!(
            entry.replay(),
            Disposition::Replayed {
                result: EffectValue::Int(99)
            }
        );
        assert!(
            !entry.replay().touched_world(),
            "replay must not touch the world"
        );
    }

    #[test]
    fn an_intent_replays_as_still_deferred() {
        let mut j = EffectJournal::new();
        j.record_intent(key(2, 0), FutureId::new(2), op(), EffectClass::Irreversible);
        assert_eq!(j.get(&key(2, 0)).unwrap().replay(), Disposition::Deferred);
    }

    #[test]
    fn a_future_sees_only_its_own_entries_in_order() {
        let mut j = EffectJournal::new();
        for (f, o) in [(1u64, 2u64), (1, 0), (1, 1), (2, 0)] {
            j.record_performed(
                key(f, o),
                FutureId::new(f),
                op(),
                EffectClass::Read,
                EffectValue::Int(o as i64),
            );
        }
        let for_one: Vec<u64> = j
            .for_future(FutureId::new(1))
            .iter()
            .map(|e| e.key.ordinal.value())
            .collect();
        assert_eq!(for_one, vec![0, 1, 2], "ordinal order, not insertion order");
        assert_eq!(j.for_future(FutureId::new(2)).len(), 1);
    }

    #[test]
    fn only_unissued_writes_are_pending_release() {
        let mut j = EffectJournal::new();
        j.record_performed(
            key(1, 0),
            FutureId::new(1),
            op(),
            EffectClass::Read,
            EffectValue::Int(1),
        );
        j.record_intent(key(1, 1), FutureId::new(1), op(), EffectClass::Irreversible);
        j.record_intent(
            key(1, 2),
            FutureId::new(1),
            op(),
            EffectClass::Compensatable,
        );
        j.record_performed(
            key(1, 3),
            FutureId::new(1),
            op(),
            EffectClass::IdempotentWrite,
            EffectValue::Unit,
        );
        let pending: Vec<u64> = j
            .pending_release(FutureId::new(1))
            .iter()
            .map(|e| e.key.ordinal.value())
            .collect();
        assert_eq!(pending, vec![1, 2]);
    }

    #[test]
    fn a_rejected_future_has_nothing_reclaimable_while_an_intent_is_open() {
        let mut j = EffectJournal::new();
        j.record_intent(key(3, 0), FutureId::new(3), op(), EffectClass::Irreversible);
        assert!(j.reclaimable(&[FutureId::new(3)]).is_empty());
        j.mark_issued(&key(3, 0), EffectValue::Unit);
        assert_eq!(j.reclaimable(&[FutureId::new(3)]), vec![key(3, 0)]);
        j.forget(&[key(3, 0)]);
        assert!(j.is_empty());
    }

    #[test]
    fn a_live_future_is_never_reclaimed() {
        let mut j = EffectJournal::new();
        j.record_performed(
            key(4, 0),
            FutureId::new(4),
            op(),
            EffectClass::Read,
            EffectValue::Int(1),
        );
        assert!(j.reclaimable(&[]).is_empty());
    }
}
