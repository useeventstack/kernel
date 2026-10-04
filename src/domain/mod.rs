//! The domain: what an execution *is*, with no knowledge of how it is stored or
//! how long it takes.
//!
//! The dependency rule is one-directional and total: `domain` depends on nothing
//! in this crate. Everything below is a value type, so the invariants the runtime
//! relies on are properties of the *types* rather than of runtime checks — a
//! future cannot mutate its parent because [`State`] is a value and the node store
//! is insert-only, not because a check would catch it.
//!
//! | module | what it defines |
//! |---|---|
//! | [`ids`] | identifiers, all allocated from counters so runs are reproducible |
//! | [`version`] | the three counters durability reasoning needs |
//! | [`state`] | the logical state, its content address, and three-way merge |
//! | [`node`] | content-addressed immutable state versions: the O(1) fork |
//! | [`delta`] | operations, branch-scoped deltas, codecs, coalescing |
//! | [`plan`] | what should happen, compiled to a flat program |
//! | [`effect`] | what may touch the outside world, and when |
//! | [`future`] | a durable alternative continuation, its lifecycle, and lineage |

pub mod delta;
pub mod effect;
pub mod future;
pub mod ids;
pub mod node;
pub mod plan;
pub mod state;
pub mod version;

pub use delta::{
    coalesce_deltas, Bytes, CompactDeltaCodec, DeltaCodec, DeltaError, EffectDelta, Operation,
    RawDeltaCodec, StateDelta,
};
pub use effect::{
    Disposition, EffectClass, EffectError, EffectKey, EffectOp, EffectRequest, EffectSink,
    EffectValue, ObservationOnlySink, RecordingSink, SuppressedAttempt, SuppressedSink,
};
pub use future::{EffectRecord, Future, FutureError, FutureStatus, FutureView, Lineage};
pub use ids::{
    AttemptId, Cursor, EffectId, EventId, ExecutionId, FutureId, IdAllocator, NodeId, PlanId,
    StepId, TRUNK,
};
pub use node::{StateNode, StateStore};
pub use plan::{Arm, Plan, PlanBuilder, PlanNode};
pub use state::{Applied, Conflict, Output, State, StateError};
pub use version::{LogPosition, Sequence, Version};
