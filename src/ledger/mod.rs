//! The ledger: the only durable artefact, and the boundary it is written to.
//!
//! ```text
//!   kernel ──records──▶ ledger ──bytes──▶ [ DurableStore port ]
//!     │                                      │
//!     └── durable prefix ◀──recovery──────────┘
//! ```
//!
//! Three parts, one direction of dependency:
//!
//! * [`record`] — *what* is written: the record set, and the invariant that
//!   every record describes exactly one step of exactly one future.
//! * [`store`] — *how* it is framed and where it lives: `LogCore` plus the
//!   in-memory and filesystem sinks behind the [`crate::ports::DurableStore`]
//!   port.
//! * [`journal`] — the effect journal, the index that turns a replayed ledger
//!   into "what did this future observe, and what did it ask the world to do".
//!
//! [`probe`] measures the real cost of the store on the machine the experiment
//! runs on, so the cost model the benchmarks use is calibrated rather than
//! assumed. It refuses to calibrate against a filesystem where durability is
//! free, because such a number would silently make every downstream result
//! meaningless.

pub mod faults;
pub mod journal;
pub mod probe;
pub mod record;
pub mod store;

pub use journal::{EffectJournal, JournalEntry};
pub use probe::{FsyncStats, GroupPoint, Probe, ProbeError, ProbeReport};
pub use record::{ForkedChild, LedgerRecord, Payload};
pub use store::{temp_ledger_dir, FileStore, LogCore, MemoryStore};
