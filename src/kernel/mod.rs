//! The execution kernel.
//!
//! ```text
//!   Plan  ──fork/select/commit──▶  [ future ] ──delta/effect──▶  Ledger
//!                                        │                          │
//!                                        └──── state ──▶ Merkle     │
//!                                                  store           │
//!                                                                   ▼
//!                                                     recovery ◀── durable prefix
//! ```
//!
//! Modules, in dependency order:
//!
//! | module | role |
//! |---|---|
//! | [`policy`] | the four knobs, and nothing else |
//! | [`durability`] | the background writer: one serial resource on the shared clock |
//! | [`evaluate`] | scoring futures and choosing one, deterministically |
//! | [`commit`] | validating a commit and merging against a moved trunk |
//! | [`recovery`] | replaying a durable ledger prefix into a valid graph |
//! | [`engine`] | the interpreter that ties them together |

pub mod commit;
pub mod durability;
pub mod engine;
pub mod evaluate;
pub mod policy;
pub mod recovery;

pub use commit::{CommitError, CommitPlan, MergeInput};
pub use durability::{Batch, Writer};
pub use engine::{Kernel, KernelError, KernelMetrics, RunReport};
pub use evaluate::{
    select, Cheapest, Evaluator, FirstByIdentity, HighestScore, LowestScore, NoViableFuture,
    OnlyWithoutIrreversible, Score, Selection,
};
pub use policy::{CommitConflict, ConflictPolicy, Durability, ExecutionPolicy};
pub use recovery::{rebuild, ExecutionGraph, RecoveryError};
