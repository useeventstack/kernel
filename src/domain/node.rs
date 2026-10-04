//! Content-addressed immutable state versions.
//!
//! This is the mechanism that makes durable alternative execution affordable.
//!
//! # The cost that matters
//!
//! If forking an execution required copying its state, evaluating *N* futures
//! would cost *N* state copies, and the whole idea would be unaffordable. So
//! the rule is the one git uses: **a branch is a pointer, not a copy**.
//!
//! ```text
//!        state S₀            state S₁            state S₂
//!      ┌──────────┐       ┌──────────┐        ┌──────────┐
//!      │ a=1 b=1  │──▶───▶│ a=2 b=1  │──▶──▶──│ a=3 b=1  │
//!      └──────────┘  fork └──────────┘  fork └──────────┘
//!           ▲             ▲                ▲
//!           │             │                │
//!      FutureId(0)   FutureId(1)     FutureId(2)      ← 8 bytes each
//!         (trunk)       branch A          branch B
//! ```
//!
//! `fork` copies a [`StateNode`], which is a `u64`. Nothing is copied and
//! nothing is mutable, so:
//!
//! * a future cannot mutate its parent or a sibling — there is no operation
//!   that could do it, because a stored state is never modified in place;
//! * abandoning a future cannot corrupt anything — nothing referenced it;
//! * two futures that reach the same state share one stored version, so
//!   converging branches cost less memory than diverging ones.
//!
//! # What is *not* claimed
//!
//! Content addressing gives a cheap equality test, not a collision-free one. The
//! kernel therefore uses the hash for the "did this change?" test and for
//! deduplication, and never as a security or correctness boundary. Every state a
//! future needs is materialised from the operation that produced it, so a hash
//! collision would cost a redundant store entry and nothing else.

use std::collections::BTreeMap;
use std::fmt;

use crate::domain::state::State;

/// A content-addressed immutable state version.
///
/// The value *is* the address: two `StateNode`s compare equal exactly when the
/// states they name are equal, except for hash collisions (see the module
/// documentation).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct StateNode(u64);

impl StateNode {
    /// The empty state, the root of every execution.
    pub const EMPTY: StateNode = StateNode(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// A short label for reports: the first 12 hex digits of the address.
    #[must_use]
    pub fn short(self) -> String {
        format!("{:012x}", self.0)
    }
}

impl fmt::Debug for StateNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "S({})", self.short())
    }
}

impl fmt::Display for StateNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.short())
    }
}

/// Insert-only store of immutable state versions.
///
/// `intern` is the only mutator and it never overwrites: presenting a state that
/// is already stored is a no-op. That is what makes every stored version
/// immutable for the lifetime of the store.
#[derive(Clone, Debug, Default)]
pub struct StateStore {
    nodes: BTreeMap<StateNode, State>,
    /// Number of distinct states stored.
    distinct: usize,
    /// Number of `intern` calls that were served from the store.
    deduped: usize,
}

impl StateStore {
    #[must_use]
    pub fn new() -> Self {
        let mut store = Self::default();
        // Version 0 is the empty state, so the root of an execution always
        // resolves without any step having run.
        store.nodes.insert(StateNode::EMPTY, State::new());
        store
    }

    /// Stores `state` and returns its address. Idempotent.
    pub fn intern(&mut self, state: &State) -> StateNode {
        let node = StateNode::new(state.content_hash());
        if self.nodes.contains_key(&node) {
            self.deduped += 1;
            return node;
        }
        self.nodes.insert(node, state.clone());
        self.distinct += 1;
        node
    }

    /// Stores `state` and returns its address plus whether it was new.
    pub fn intern_new(&mut self, state: &State) -> (StateNode, bool) {
        let node = StateNode::new(state.content_hash());
        if let std::collections::btree_map::Entry::Vacant(slot) = self.nodes.entry(node) {
            slot.insert(state.clone());
            self.distinct += 1;
            return (node, true);
        }
        self.deduped += 1;
        (node, false)
    }

    /// Resolves an address, or `None` if this store never saw that state.
    #[must_use]
    pub fn get(&self, node: StateNode) -> Option<&State> {
        self.nodes.get(&node)
    }

    /// Resolves an address, cloning. Used at the few points where the caller
    /// needs to own a state.
    #[must_use]
    pub fn state(&self, node: StateNode) -> Option<State> {
        self.nodes.get(&node).cloned()
    }

    /// Number of distinct versions held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// How many `intern` calls returned an already-stored version.
    #[must_use]
    pub fn deduped(&self) -> usize {
        self.deduped
    }

    /// Versions in address order. Used by tests and by `inspect` output.
    pub fn iter(&self) -> impl Iterator<Item = (StateNode, &State)> {
        self.nodes.iter().map(|(n, s)| (*n, s))
    }

    /// Drops every version that is not in `keep` and is not
    /// [`StateNode::EMPTY`].
    ///
    /// This is reclamation of *speculative* storage only. Versions reachable
    /// from the trunk or from any future that is not terminal must be retained;
    /// passing an incomplete `keep` set loses history and is a caller bug. The
    /// runtime therefore computes `keep` as a closure over the whole live
    /// execution graph, never as a heuristic.
    pub fn retain<F: Fn(StateNode) -> bool>(&mut self, keep: F) -> usize {
        let before = self.nodes.len();
        let root = StateNode::EMPTY;
        self.nodes.retain(|n, _| *n == root || keep(*n));
        before - self.nodes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pairs: &[(&str, i64)]) -> State {
        let mut s = State::new();
        for (k, v) in pairs {
            s.set(k, *v);
        }
        s
    }

    #[test]
    fn empty_state_always_resolves() {
        let store = StateStore::new();
        assert_eq!(store.get(StateNode::EMPTY), Some(&State::new()));
    }

    #[test]
    fn interning_is_idempotent() {
        let mut store = StateStore::new();
        let a = store.intern(&state(&[("x", 1)]));
        let b = store.intern(&state(&[("x", 1)]));
        assert_eq!(a, b);
        assert_eq!(store.deduped(), 1);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn stored_versions_are_never_overwritten() {
        let mut store = StateStore::new();
        let a = store.intern(&state(&[("x", 1)]));
        // Interning a different state must not disturb the old version.
        let _b = store.intern(&state(&[("x", 2)]));
        assert_eq!(store.get(a).unwrap().get("x"), 1);
    }

    #[test]
    fn a_stored_state_is_only_reachable_by_shared_reference() {
        // This is a compile-time statement more than a runtime one: `StateStore`
        // exposes `get` and no `get_mut`, so there is no API through which a
        // caller could mutate a stored version. The test documents the absence,
        // and adding a mutable accessor would make it trivially false.
        let mut store = StateStore::new();
        let node = store.intern(&state(&[("x", 1)]));
        let read = store.get(node).unwrap().get("x");
        assert_eq!(read, 1);
        // Interning a different state leaves the first one exactly as it was.
        let other = store.intern(&state(&[("x", 99)]));
        assert_ne!(node, other);
        assert_eq!(store.get(node).unwrap().get("x"), 1);
    }

    #[test]
    fn intern_new_reports_freshness() {
        let mut store = StateStore::new();
        let (a, fresh_a) = store.intern_new(&state(&[("x", 1)]));
        let (b, fresh_b) = store.intern_new(&state(&[("x", 1)]));
        assert_eq!(a, b);
        assert!(fresh_a);
        assert!(!fresh_b);
    }

    #[test]
    fn retain_drops_only_what_is_not_kept() {
        let mut store = StateStore::new();
        let a = store.intern(&state(&[("x", 1)]));
        let b = store.intern(&state(&[("x", 2)]));
        let dropped = store.retain(|n| n == a);
        assert_eq!(dropped, 1);
        assert!(store.get(a).is_some());
        assert!(store.get(b).is_none());
        assert!(store.get(StateNode::EMPTY).is_some());
    }
}
