//! Identifier types for the execution domain.
//!
//! Every identifier is a plain `u64` newtype. They are *not* random: the
//! runtime is fully deterministic, so identifiers are allocated from counters
//! owned by the kernel (see [`IdAllocator`]). Determinism matters twice over
//! here — it makes the tests reproducible, and it makes an idempotency key
//! derived from a future's identity stable across retries and restarts.

use std::fmt;

macro_rules! define_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(u64);

        impl $name {
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn value(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }
    };
}

define_id!(
    PlanId,
    "Stable identifier of a plan definition (a workflow)."
);
define_id!(
    ExecutionId,
    "Stable identifier of one execution of a plan. An execution owns the trunk."
);
define_id!(
    FutureId,
    "Identifier of one durable alternative continuation. The trunk is a future too:\
     it is [`FutureId::TRUNK`]."
);
define_id!(StepId, "Position of a step inside a compiled plan.");
define_id!(
    AttemptId,
    "Increments every time a step is re-executed after a recovery or a rollback."
);
define_id!(
    NodeId,
    "Index of a node inside a compiled plan; also the position of the interpreter's\
     cursor."
);

/// Cursor into a compiled plan: which node runs next.
///
/// The plan is flattened at construction time, so a cursor is a single `u32`
/// and cursor arithmetic (`+ 1` to advance inside a sequence) is exact. The
/// cursor is written into every ledger record, which is what lets recovery
/// resume a future at the right place without re-deriving anything.
pub type Cursor = NodeId;

/// The root of every execution. Forking from the trunk yields real futures;
/// the trunk itself is a future that is already authoritative.
pub const TRUNK: FutureId = FutureId::new(0);

define_id!(EffectId, "Ordinal of an effect within one future.");
define_id!(
    EventId,
    "Monotonic identifier of an emitted observable event."
);

/// Deterministic identifier allocator.
#[derive(Clone, Debug, Default)]
pub struct IdAllocator {
    next: u64,
}

impl IdAllocator {
    #[must_use]
    pub fn new(start: u64) -> Self {
        Self { next: start }
    }

    /// The first identifier this allocator hands out.
    #[must_use]
    pub fn peek(&self) -> u64 {
        self.next
    }

    pub fn bump(&mut self) -> u64 {
        let value = self.next;
        self.next += 1;
        value
    }

    pub fn plan(&mut self) -> PlanId {
        PlanId::new(self.bump())
    }

    pub fn execution(&mut self) -> ExecutionId {
        ExecutionId::new(self.bump())
    }

    /// Allocated futures start at 1: 0 is [`TRUNK`].
    pub fn future(&mut self) -> FutureId {
        FutureId::new(self.bump() + 1)
    }

    pub fn event(&mut self) -> EventId {
        EventId::new(self.bump())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_ordered_and_displayed() {
        assert!(PlanId::new(1) < PlanId::new(2));
        assert_eq!(PlanId::new(7).to_string(), "PlanId(7)");
        assert_eq!(ExecutionId::from(3u64), ExecutionId::new(3));
    }

    #[test]
    fn allocator_is_deterministic() {
        let mut ids = IdAllocator::new(1);
        assert_eq!(ids.plan(), PlanId::new(1));
        assert_eq!(ids.execution(), ExecutionId::new(2));
        assert_eq!(ids.event(), EventId::new(3));
        // `future` reserves 0 for the trunk, so it skips the value `bump` returns.
        assert_eq!(ids.future(), FutureId::new(5));
    }

    #[test]
    fn trunk_is_never_allocated_as_a_future() {
        let mut ids = IdAllocator::new(0);
        for _ in 0..32 {
            assert_ne!(ids.future(), TRUNK);
        }
    }
}
