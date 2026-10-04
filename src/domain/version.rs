//! Versions, sequences and log positions.
//!
//! Three monotonic counters are needed to reason about durability:
//!
//! * [`Version`]  – the speculative state version of an execution. Every
//!   executed step produces exactly one new version. Version `0` is the
//!   "nothing has been executed" version.
//! * [`Sequence`] – the position of a durable log record inside one execution,
//!   starting at `0`.
//! * [`LogPosition`] – a byte offset in the physical append-only log.
//!
//! Invariant 4 ("the durable frontier must never move backward") is enforced
//! by [`Version::checked_advance_to`], which is the only way a frontier is
//! allowed to move forward.

use std::fmt;

/// Monotonic speculative/durable state version of an execution.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Version(u64);

impl Version {
    /// Version of an execution that has not executed any step yet.
    pub const ZERO: Version = Version(0);
    /// First version produced by an executed step.
    pub const FIRST: Version = Version(1);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Version immediately after `self`.
    #[must_use]
    pub fn next(self) -> Version {
        Version(self.0 + 1)
    }

    #[must_use]
    pub fn is_after(self, other: Version) -> bool {
        self.0 > other.0
    }

    /// The `distance` from `other` to `self`, or `None` if `self` is not after
    /// `other`. Used to measure rollback distance.
    #[must_use]
    pub fn distance_from(self, other: Version) -> Option<u64> {
        self.0.checked_sub(other.0)
    }

    /// Move a frontier forward to `target`.
    ///
    /// Returns `false` when `target` is *behind* the current version, which is
    /// the condition forbidden by invariant 4. Callers treat that as a bug and
    /// surface it rather than silently accepting it.
    pub fn checked_advance_to(&mut self, target: Version) -> bool {
        if target.0 >= self.0 {
            self.0 = target.0;
            true
        } else {
            false
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "V{}", self.0)
    }
}

impl fmt::Debug for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Position of a record inside the durable log of one execution.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Sequence(u64);

impl Sequence {
    pub const ZERO: Sequence = Sequence(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn next(self) -> Sequence {
        Sequence(self.0 + 1)
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "S{}", self.0)
    }
}

impl fmt::Debug for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Byte offset of a record inside the physical append-only log file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LogPosition(u64);

impl LogPosition {
    pub const START: LogPosition = LogPosition(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LogPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}

impl fmt::Debug for LogPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontier_never_moves_backward() {
        let mut v = Version::ZERO;
        assert!(v.checked_advance_to(Version::new(3)));
        assert!(v.checked_advance_to(Version::new(3)));
        assert!(!v.checked_advance_to(Version::new(2)));
        assert_eq!(v, Version::new(3));
    }

    #[test]
    fn distance_is_none_when_not_after() {
        assert_eq!(Version::new(5).distance_from(Version::new(2)), Some(3));
        assert_eq!(Version::new(2).distance_from(Version::new(2)), Some(0));
        assert_eq!(Version::new(1).distance_from(Version::new(2)), None);
    }

    #[test]
    fn versions_and_sequences_increment() {
        assert_eq!(Version::new(1).next(), Version::new(2));
        assert_eq!(Sequence::ZERO.next(), Sequence::new(1));
    }
}
