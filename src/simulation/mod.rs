//! Deterministic simulation support: virtual clock and failure injection.

pub mod clock;
pub mod failure;

pub use clock::{as_millis_f64, format_ms, format_ms_ceil, from_millis_f64, SimClock};
pub use failure::{Checkpoint, FailureInjector, FailurePoint, InjectedFailure};
