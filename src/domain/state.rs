//! The logical workflow state, its content address, and three-way merge.
//!
//! Two properties of this type carry most of the weight of the whole design:
//!
//! 1. **It is a value.** There is no interior mutability and no shared
//!    reference, so *a future cannot mutate its parent or a sibling* is not an
//!    invariant that has to be enforced — it is unrepresentable. Isolation is a
//!    property of the type system, not of a runtime check.
//! 2. **It is content addressed.** [`State::content_hash`] is a pure function of
//!    the entries, so two states that are logically equal hash equally. That
//!    makes "did this future change anything?" an integer comparison, and it is
//!    what makes branch creation free (see [`crate::domain::node`]).
//!
//! Everything outside the runtime (persistence format, tests, benchmarks) reads
//! state through the same accessors, so no code path can disagree about what a
//! state is.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::domain::version::Version;

/// Key used by the emit operation to expose a value to the outside world.
pub const EMIT_KEY: &str = "__emit__";

/// Deterministic key/value workflow state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    entries: BTreeMap<String, i64>,
}

/// Result of applying one operation to a state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// Simulated time the operation consumed.
    pub slept: std::time::Duration,
    /// Name of the emitted event, if the operation was [`crate::domain::Operation::Emit`].
    pub emitted: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    /// Integer overflow while applying an arithmetic operation.
    Overflow { key: String },
    /// The operation has no state transition (failing is control flow).
    NotAStateOperation { op: &'static str },
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::Overflow { key } => {
                write!(f, "integer overflow while updating key `{key}`")
            }
            StateError::NotAStateOperation { op } => {
                write!(f, "operation `{op}` does not have a state transition")
            }
        }
    }
}

impl std::error::Error for StateError {}

impl State {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Value of `key`; missing keys are `0`.
    ///
    /// The state is deliberately *total*: every key reads as an integer, so a
    /// workflow cannot distinguish "never written" from "written as zero". This
    /// is what makes a dropped identity delta safe — see
    /// [`crate::domain::coalesce`].
    #[must_use]
    pub fn get(&self, key: &str) -> i64 {
        self.entries.get(key).copied().unwrap_or(0)
    }

    pub fn set(&mut self, key: &str, value: i64) {
        self.entries.insert(key.to_owned(), value);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &i64)> {
        self.entries.iter()
    }

    /// Every key this state mentions, sorted. This is the universe a
    /// three-way merge iterates over.
    #[must_use]
    pub fn keys(&self) -> BTreeSet<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// Stable 64-bit content address of the whole state.
    ///
    /// Computed over the *canonical* encoding: keys in `BTreeMap` order, so it
    /// is independent of insertion order. Two states that are `==` always hash
    /// equally; the converse is not claimed, because a hash collision is
    /// possible in principle. The kernel never relies on the converse for
    /// correctness — only for the cheap "did anything change?" test and for
    /// deduplicating identical versions in the node store.
    #[must_use]
    pub fn content_hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for (key, value) in &self.entries {
            feed(&(key.len() as u64).to_le_bytes());
            feed(key.as_bytes());
            feed(&value.to_le_bytes());
        }
        h
    }

    /// Applies one operation in place.
    ///
    /// [`crate::domain::Operation::Fail`] is *not* handled here: failing is a
    /// control-flow decision owned by the kernel, not a state transition.
    ///
    /// Nondeterminism is not an operation either. A step cannot obtain a value
    /// from the world except by requesting an
    /// [`crate::domain::EffectClass::Read`], and the runtime folds the journalled
    /// result into state itself. That is why this function is total: there is no
    /// variant that could resolve differently on a second execution.
    pub fn apply(&mut self, op: &crate::domain::Operation) -> Result<Applied, StateError> {
        use crate::domain::Operation as Op;
        let applied = match op {
            Op::Set { key, value } => {
                self.entries.insert(key.clone(), *value);
                Applied {
                    slept: ZERO,
                    emitted: None,
                }
            }
            Op::Add { key, by } => {
                let next = self
                    .get(key)
                    .checked_add(*by)
                    .ok_or_else(|| StateError::Overflow { key: key.clone() })?;
                self.entries.insert(key.clone(), next);
                Applied {
                    slept: ZERO,
                    emitted: None,
                }
            }
            Op::Compute { key, scale, addend } => {
                let scaled = self
                    .get(key)
                    .checked_mul(*scale)
                    .ok_or_else(|| StateError::Overflow { key: key.clone() })?;
                let next = scaled
                    .checked_add(*addend)
                    .ok_or_else(|| StateError::Overflow { key: key.clone() })?;
                self.entries.insert(key.clone(), next);
                Applied {
                    slept: ZERO,
                    emitted: None,
                }
            }
            Op::Sleep { ms } => Applied {
                slept: std::time::Duration::from_millis(*ms),
                emitted: None,
            },
            Op::Emit { event } => {
                self.entries
                    .entry(EMIT_KEY.to_owned())
                    .and_modify(|v| *v += 1)
                    .or_insert(1);
                Applied {
                    slept: ZERO,
                    emitted: Some(event.clone()),
                }
            }
            Op::Fail { .. } => return Err(StateError::NotAStateOperation { op: "fail" }),
        };
        Ok(applied)
    }

    /// Externally visible output counter.
    #[must_use]
    pub fn output(&self) -> i64 {
        self.get(EMIT_KEY)
    }

    /// Logical equality: every key either state defines reads back the same.
    ///
    /// Needed because dropping a no-op delta run can leave a key *absent* where
    /// the unfolded sequence would have materialised it as `0`. The state is
    /// total, so those are the same value, but the structural `PartialEq` can
    /// tell them apart.
    #[must_use]
    pub fn logical_eq(&self, other: &State) -> bool {
        self.keys().into_iter().chain(other.keys()).all(|k| {
            self.get(k) == other.get(k)
                && self.entries.contains_key(k) == other.entries.contains_key(k)
        })
    }
}

const ZERO: std::time::Duration = std::time::Duration::ZERO;

// ---------------------------------------------------------------------------
// Three-way merge
// ---------------------------------------------------------------------------

/// One key both sides changed, incompatibly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub key: String,
    /// The value at the common base.
    pub base: i64,
    /// The value the future proposes.
    pub ours: i64,
    /// The value the trunk currently holds.
    pub theirs: i64,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "key `{}`: future wants {}, trunk has {}, both changed from {}",
            self.key, self.ours, self.theirs, self.base
        )
    }
}

impl State {
    /// Three-way merge, the same shape as a `git merge-base` comparison.
    ///
    /// `base` is the state the future forked from, `ours` is the future's head
    /// and `theirs` is the trunk's current head. For every key:
    ///
    /// | base | ours | theirs | result |
    /// |---|---|---|---|
    /// | *b* | *b* | anything | *theirs* — we did not change it |
    /// | anything | *o* | *b* | *o* — they did not change it |
    /// | *b* | *o* | *o* | *o* — convergent, no conflict |
    /// | *b* | *o* | *t* , *o* ≠ *t* | **conflict** |
    ///
    /// Because the state is total, an absent key reads as `0`, so "did the
    /// future write this key?" is answered by `ours != base` rather than by
    /// tracking a write set separately. That is why no write-set bookkeeping is
    /// needed for a correct merge; the write set is still tracked, but only as
    /// an observable and for reporting.
    pub fn three_way_merge(
        base: &State,
        ours: &State,
        theirs: &State,
    ) -> Result<State, Vec<Conflict>> {
        let mut out = theirs.clone();
        let mut conflicts = Vec::new();
        for key in ours
            .keys()
            .into_iter()
            .chain(theirs.keys())
            .collect::<BTreeSet<_>>()
        {
            let b = base.get(key);
            let o = ours.get(key);
            let t = theirs.get(key);
            if o == b {
                // The future did not change this key.
                continue;
            }
            if t == b || o == t {
                out.set(key, o);
            } else {
                conflicts.push(Conflict {
                    key: key.to_owned(),
                    base: b,
                    ours: o,
                    theirs: t,
                });
            }
        }
        if conflicts.is_empty() {
            Ok(out)
        } else {
            Err(conflicts)
        }
    }

    /// Keys whose value differs between `self` and `other`. Used for reporting
    /// what a commit actually changed.
    #[must_use]
    pub fn diff(&self, other: &State) -> Vec<(String, i64, i64)> {
        self.keys()
            .into_iter()
            .chain(other.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|k| {
                let a = self.get(k);
                let b = other.get(k);
                (a != b).then(|| (k.to_owned(), a, b))
            })
            .collect()
    }
}

/// Output crossing the visibility boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub version: Version,
    pub name: String,
    pub value: i64,
    pub state_digest: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Operation as Op;

    #[test]
    fn add_and_compute_accumulate() {
        let mut s = State::new();
        s.apply(&Op::Add {
            key: "counter".into(),
            by: 1,
        })
        .unwrap();
        s.apply(&Op::Add {
            key: "counter".into(),
            by: 1,
        })
        .unwrap();
        s.apply(&Op::Compute {
            key: "counter".into(),
            scale: 10,
            addend: -4,
        })
        .unwrap();
        assert_eq!(s.get("counter"), 16);
    }

    #[test]
    fn set_overwrites_missing_key() {
        let mut s = State::new();
        s.set("k", 9);
        assert_eq!(s.get("k"), 9);
    }

    #[test]
    fn emit_counts_and_advances_sleep() {
        let mut s = State::new();
        let a = s
            .apply(&Op::Emit {
                event: "result".into(),
            })
            .unwrap();
        assert_eq!(a.emitted.as_deref(), Some("result"));
        assert_eq!(s.output(), 1);
        let a = s.apply(&Op::Sleep { ms: 25 }).unwrap();
        assert_eq!(a.slept, std::time::Duration::from_millis(25));
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        let mut s = State::new();
        s.set("k", i64::MAX);
        assert!(matches!(
            s.apply(&Op::Add {
                key: "k".into(),
                by: 1
            }),
            Err(StateError::Overflow { .. })
        ));
    }

    #[test]
    fn content_hash_is_order_independent_and_sensitive() {
        let mut a = State::new();
        a.set("x", 1);
        a.set("y", 2);
        let mut b = State::new();
        b.set("y", 2);
        b.set("x", 1);
        assert_eq!(a.content_hash(), b.content_hash());
        b.set("y", 3);
        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn merge_takes_our_change_when_they_did_not_move() {
        let mut base = State::new();
        base.set("shared", 1);
        let mut ours = base.clone();
        ours.set("shared", 2);
        let theirs = base.clone();
        let merged = State::three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.get("shared"), 2);
    }

    #[test]
    fn merge_keeps_their_change_when_we_did_not_move() {
        let mut base = State::new();
        base.set("shared", 1);
        let ours = base.clone();
        let mut theirs = base.clone();
        theirs.set("shared", 9);
        let merged = State::three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.get("shared"), 9);
    }

    #[test]
    fn merge_of_convergent_changes_is_not_a_conflict() {
        let mut base = State::new();
        base.set("shared", 1);
        let mut ours = base.clone();
        ours.set("shared", 5);
        let mut theirs = base.clone();
        theirs.set("shared", 5);
        let merged = State::three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.get("shared"), 5);
    }

    #[test]
    fn merge_reports_a_real_conflict() {
        let mut base = State::new();
        base.set("shared", 1);
        let mut ours = base.clone();
        ours.set("shared", 5);
        let mut theirs = base.clone();
        theirs.set("shared", 7);
        let err = State::three_way_merge(&base, &ours, &theirs).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].key, "shared");
        assert_eq!(err[0].base, 1);
        assert_eq!(err[0].ours, 5);
        assert_eq!(err[0].theirs, 7);
    }

    #[test]
    fn merge_keeps_independent_keys_from_both_sides() {
        let base = State::new();
        let mut ours = State::new();
        ours.set("a", 1);
        let mut theirs = State::new();
        theirs.set("b", 2);
        let merged = State::three_way_merge(&base, &ours, &theirs).unwrap();
        assert_eq!(merged.get("a"), 1);
        assert_eq!(merged.get("b"), 2);
    }

    #[test]
    fn diff_reports_only_changed_keys() {
        let mut a = State::new();
        a.set("x", 1);
        a.set("y", 2);
        let mut b = a.clone();
        b.set("y", 3);
        b.set("z", 4);
        assert_eq!(
            a.diff(&b),
            vec![("y".to_owned(), 2, 3), ("z".to_owned(), 0, 4)]
        );
    }
}
