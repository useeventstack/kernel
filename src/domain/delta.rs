//! Operations, branch-scoped deltas, and the delta codecs.
//!
//! A [`StateDelta`] is the only thing the runtime records about a pure step: it
//! stores the *transition*, not a snapshot. Replaying an ordered delta chain
//! against a base state reconstructs any later state exactly, which is what
//! makes both the append-log baseline and the Merkle node store cheap.
//!
//! Two things changed relative to a linear log, and both follow from futures
//! being first class:
//!
//! * a delta names the **future** it belongs to, not an execution — one
//!   execution has many concurrent delta chains;
//! * a delta names the version it was **applied to** (`parent`), so a chain is
//!   self-describing and recovery can detect a gap without consulting any other
//!   structure.
//!
//! Two codecs are implemented so that storage is a *measurement*, not an
//! assertion: [`RawDeltaCodec`] (fixed width, one delta per operation) and
//! [`CompactDeltaCodec`] (varint / zigzag of the coalesced chain).
//!
//! Coalescing folds a run of adjacent same-key operations into one canonical
//! [`Operation::Compute`]. Every state operation is an affine map
//! `x -> scale * x + addend`, and a composition of affine maps is affine, so
//! folding is semantics preserving. Runs are broken by any operation with a
//! side effect ([`Operation::is_barrier`]).

use std::fmt;

use crate::domain::ids::{AttemptId, EffectId, FutureId, StepId};

/// Owned byte buffer.
pub type Bytes = Vec<u8>;

/// A single state transition, or a control-flow marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Assign `value` to `key`.
    Set { key: String, value: i64 },
    /// Add `by` to `key`.
    Add { key: String, by: i64 },
    /// `key = key * scale + addend`.
    Compute {
        key: String,
        scale: i64,
        addend: i64,
    },
    /// Advance the simulated clock. Pure for state, never coalesced.
    Sleep { ms: u64 },
    /// Emit an externally visible event. Never coalesced.
    Emit { event: String },
    /// Deterministically fail the execution at this step. Never coalesced.
    Fail { reason: String },
}

impl Operation {
    /// Affine form `x -> scale * x + addend` if the operation only rewrites a
    /// single key, widened to `i128` so composition cannot overflow.
    #[must_use]
    pub fn affine(&self) -> Option<(&str, i128, i128)> {
        match self {
            Operation::Set { key, value } => Some((key.as_str(), 0, i128::from(*value))),
            Operation::Add { key, by } => Some((key.as_str(), 1, i128::from(*by))),
            Operation::Compute { key, scale, addend } => {
                Some((key.as_str(), i128::from(*scale), i128::from(*addend)))
            }
            Operation::Sleep { .. } | Operation::Emit { .. } | Operation::Fail { .. } => None,
        }
    }

    /// True when the operation has a side effect beyond rewriting a key.
    #[must_use]
    pub fn is_barrier(&self) -> bool {
        self.affine().is_none()
    }

    /// Key rewritten by the operation, if any.
    #[must_use]
    pub fn touched_key(&self) -> Option<&str> {
        self.affine().map(|(key, _, _)| key)
    }

    /// Encoded size, used for storage accounting.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        CompactDeltaCodec.encode_op(self).len()
    }

    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Operation::Set { key, value } => format!("set({key} = {value})"),
            Operation::Add { key, by } => format!("add({key} += {by})"),
            Operation::Compute { key, scale, addend } => {
                format!("compute({key} = {key} * {scale} + {addend})")
            }
            Operation::Sleep { ms } => format!("sleep({ms}ms)"),
            Operation::Emit { event } => format!("emit({event})"),
            Operation::Fail { reason } => format!("fail({reason})"),
        }
    }
}

/// One executed pure step on one future, reduced to a transition.
///
/// `parent` is the version this delta was applied to and `version` is the
/// version it produced, so a future's chain is verifiable on its own: a chain is
/// well formed iff every `parent` equals the previous delta's `version` and the
/// first `parent` is the version the future forked at. [`crate::kernel::recovery`]
/// relies on exactly this, and it is how "the runtime never recovers into a
/// state that could not have been produced by a valid execution" is checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateDelta {
    pub future: FutureId,
    pub step: StepId,
    pub attempt: AttemptId,
    /// Version the delta was applied to.
    pub parent: Version,
    /// Version the delta produced.
    pub version: Version,
    pub operation: Operation,
}

use crate::domain::version::Version;

impl StateDelta {
    #[must_use]
    pub fn new(
        future: FutureId,
        step: StepId,
        attempt: AttemptId,
        parent: Version,
        version: Version,
        operation: Operation,
    ) -> Self {
        Self {
            future,
            step,
            attempt,
            parent,
            version,
            operation,
        }
    }

    /// Whether `self` continues directly from `previous`.
    #[must_use]
    pub fn follows(&self, previous: &StateDelta) -> bool {
        self.future == previous.future && self.parent == previous.version
    }
}

/// One journalled effect occurrence. The mirror image of [`StateDelta`]: an
/// effect produces no state transition of its own, but it does produce an
/// externally visible fact that has to survive a rollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectDelta {
    pub future: FutureId,
    pub ordinal: EffectId,
    pub attempt: AttemptId,
    /// Version the effect was requested at.
    pub parent: Version,
    /// Version the request produced.
    pub version: Version,
    pub class: crate::domain::effect::EffectClass,
    pub op: crate::domain::effect::EffectOp,
    pub reads_into: Option<String>,
    pub writes: Option<String>,
    pub compensation: Option<crate::domain::effect::EffectOp>,
    /// What the runtime did, and with what result.
    pub disposition: crate::domain::effect::Disposition,
}

impl EffectDelta {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        future: FutureId,
        ordinal: EffectId,
        attempt: AttemptId,
        parent: Version,
        version: Version,
        class: crate::domain::effect::EffectClass,
        op: crate::domain::effect::EffectOp,
        disposition: crate::domain::effect::Disposition,
    ) -> Self {
        Self {
            future,
            ordinal,
            attempt,
            parent,
            version,
            class,
            op,
            reads_into: None,
            writes: None,
            compensation: None,
            disposition,
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
    pub fn with_compensation(mut self, op: crate::domain::effect::EffectOp) -> Self {
        self.compensation = Some(op);
        self
    }
}

/// Encoding of a delta or an effect record.
pub trait DeltaCodec: fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;

    fn encode(&self, delta: &StateDelta) -> Bytes;
    fn encode_op(&self, op: &Operation) -> Bytes;
    fn decode(&self, bytes: &[u8]) -> Result<StateDelta, DeltaError>;

    /// Folds adjacent same-key operations into a single canonical operation.
    fn coalesce(&self, deltas: &[StateDelta]) -> Vec<StateDelta>;

    fn encoded_len(&self, deltas: &[StateDelta]) -> usize {
        deltas.iter().map(|d| self.encode(d).len()).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaError {
    UnexpectedEnd,
    UnknownTag(u8),
    InvalidUtf8,
    TrailingBytes(usize),
}

impl fmt::Display for DeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeltaError::UnexpectedEnd => write!(f, "delta ended unexpectedly"),
            DeltaError::UnknownTag(t) => write!(f, "unknown operation tag {t}"),
            DeltaError::InvalidUtf8 => write!(f, "delta contained invalid utf-8"),
            DeltaError::TrailingBytes(n) => write!(f, "delta had {n} trailing bytes"),
        }
    }
}

impl std::error::Error for DeltaError {}

const TAG_SET: u8 = 0;
const TAG_ADD: u8 = 1;
const TAG_COMPUTE: u8 = 2;
const TAG_SLEEP: u8 = 3;
const TAG_EMIT: u8 = 4;
const TAG_FAIL: u8 = 5;

fn delta_tag(op: &Operation) -> u8 {
    match op {
        Operation::Set { .. } => TAG_SET,
        Operation::Add { .. } => TAG_ADD,
        Operation::Compute { .. } => TAG_COMPUTE,
        Operation::Sleep { .. } => TAG_SLEEP,
        Operation::Emit { .. } => TAG_EMIT,
        Operation::Fail { .. } => TAG_FAIL,
    }
}

/// Fixed width codec: the unoptimised storage measurement.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawDeltaCodec;

impl DeltaCodec for RawDeltaCodec {
    fn name(&self) -> &'static str {
        "raw_fixed_width"
    }

    fn encode(&self, delta: &StateDelta) -> Bytes {
        let mut out = Vec::with_capacity(64);
        out.push(delta_tag(&delta.operation));
        out.extend_from_slice(&delta.future.value().to_le_bytes());
        out.extend_from_slice(&delta.step.value().to_le_bytes());
        out.extend_from_slice(&delta.attempt.value().to_le_bytes());
        out.extend_from_slice(&delta.parent.value().to_le_bytes());
        out.extend_from_slice(&delta.version.value().to_le_bytes());
        match &delta.operation {
            Operation::Set { key, value } => {
                push_str_wide(&mut out, key);
                out.extend_from_slice(&value.to_le_bytes());
            }
            Operation::Add { key, by } => {
                push_str_wide(&mut out, key);
                out.extend_from_slice(&by.to_le_bytes());
            }
            Operation::Compute { key, scale, addend } => {
                push_str_wide(&mut out, key);
                out.extend_from_slice(&scale.to_le_bytes());
                out.extend_from_slice(&addend.to_le_bytes());
            }
            Operation::Sleep { ms } => out.extend_from_slice(&ms.to_le_bytes()),
            Operation::Emit { event } => push_str_wide(&mut out, event),
            Operation::Fail { reason } => push_str_wide(&mut out, reason),
        }
        out
    }

    fn encode_op(&self, op: &Operation) -> Bytes {
        let mut out = Vec::with_capacity(24);
        out.push(delta_tag(op));
        match op {
            Operation::Set { key, value } => {
                push_str_wide(&mut out, key);
                out.extend_from_slice(&value.to_le_bytes());
            }
            Operation::Add { key, by } => {
                push_str_wide(&mut out, key);
                out.extend_from_slice(&by.to_le_bytes());
            }
            Operation::Compute { key, scale, addend } => {
                push_str_wide(&mut out, key);
                out.extend_from_slice(&scale.to_le_bytes());
                out.extend_from_slice(&addend.to_le_bytes());
            }
            Operation::Sleep { ms } => out.extend_from_slice(&ms.to_le_bytes()),
            Operation::Emit { event } => push_str_wide(&mut out, event),
            Operation::Fail { reason } => push_str_wide(&mut out, reason),
        }
        out
    }

    fn decode(&self, bytes: &[u8]) -> Result<StateDelta, DeltaError> {
        let mut r = Reader::new(bytes);
        let tag = r.u8()?;
        let future = FutureId::new(r.u64()?);
        let step = StepId::new(r.u64()?);
        let attempt = AttemptId::new(r.u64()?);
        let parent = Version::new(r.u64()?);
        let version = Version::new(r.u64()?);
        let operation = decode_op(&mut r, tag, Width::Fixed)?;
        r.finish()?;
        Ok(StateDelta::new(
            future, step, attempt, parent, version, operation,
        ))
    }

    fn coalesce(&self, deltas: &[StateDelta]) -> Vec<StateDelta> {
        coalesce_deltas(deltas)
    }
}

/// Varint / zigzag codec: compact for the small counters the runtime uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompactDeltaCodec;

impl DeltaCodec for CompactDeltaCodec {
    fn name(&self) -> &'static str {
        "compact_varint"
    }

    fn encode(&self, delta: &StateDelta) -> Bytes {
        let mut out = Vec::with_capacity(16);
        out.push(delta_tag(&delta.operation));
        varint::put_u64(&mut out, delta.future.value());
        varint::put_u64(&mut out, delta.step.value());
        varint::put_u64(&mut out, delta.attempt.value());
        varint::put_u64(&mut out, delta.parent.value());
        varint::put_u64(&mut out, delta.version.value());
        encode_op_into(&mut out, &delta.operation, varint::put_str);
        out
    }

    fn encode_op(&self, op: &Operation) -> Bytes {
        let mut out = Vec::with_capacity(16);
        out.push(delta_tag(op));
        encode_op_into(&mut out, op, varint::put_str);
        out
    }

    fn decode(&self, bytes: &[u8]) -> Result<StateDelta, DeltaError> {
        let mut r = Reader::new(bytes);
        let tag = r.u8()?;
        let future = FutureId::new(r.varint_u64()?);
        let step = StepId::new(r.varint_u64()?);
        let attempt = AttemptId::new(r.varint_u64()?);
        let parent = Version::new(r.varint_u64()?);
        let version = Version::new(r.varint_u64()?);
        let operation = decode_op(&mut r, tag, Width::Varint)?;
        r.finish()?;
        Ok(StateDelta::new(
            future, step, attempt, parent, version, operation,
        ))
    }

    fn coalesce(&self, deltas: &[StateDelta]) -> Vec<StateDelta> {
        coalesce_deltas(deltas)
    }
}

fn encode_op_into<F: Fn(&mut Bytes, &str)>(out: &mut Bytes, op: &Operation, put_str: F) {
    match op {
        Operation::Set { key, value } => {
            put_str(out, key);
            varint::put_i64(out, *value);
        }
        Operation::Add { key, by } => {
            put_str(out, key);
            varint::put_i64(out, *by);
        }
        Operation::Compute { key, scale, addend } => {
            put_str(out, key);
            varint::put_i64(out, *scale);
            varint::put_i64(out, *addend);
        }
        Operation::Sleep { ms } => varint::put_u64(out, *ms),
        Operation::Emit { event } => put_str(out, event),
        Operation::Fail { reason } => put_str(out, reason),
    }
}

/// Integer and string width used by a codec.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Width {
    /// Fixed width: `u32`-prefixed strings, `i64` integers.
    Fixed,
    /// Varint: length-prefixed strings, zigzag integers.
    Varint,
}

impl Width {
    fn read_str(self, r: &mut Reader<'_>) -> Result<String, DeltaError> {
        match self {
            Width::Fixed => r.str_wide(),
            Width::Varint => r.varint_str(),
        }
    }

    fn read_i64(self, r: &mut Reader<'_>) -> Result<i64, DeltaError> {
        match self {
            Width::Fixed => r.i64(),
            Width::Varint => r.varint_i64(),
        }
    }

    fn read_u64(self, r: &mut Reader<'_>) -> Result<u64, DeltaError> {
        match self {
            Width::Fixed => r.u64(),
            Width::Varint => r.varint_u64(),
        }
    }
}

/// Decodes one operation. Shared by both codecs, which differ only in width.
fn decode_op(r: &mut Reader<'_>, tag: u8, w: Width) -> Result<Operation, DeltaError> {
    Ok(match tag {
        TAG_SET => Operation::Set {
            key: w.read_str(r)?,
            value: w.read_i64(r)?,
        },
        TAG_ADD => Operation::Add {
            key: w.read_str(r)?,
            by: w.read_i64(r)?,
        },
        TAG_COMPUTE => Operation::Compute {
            key: w.read_str(r)?,
            scale: w.read_i64(r)?,
            addend: w.read_i64(r)?,
        },
        TAG_SLEEP => Operation::Sleep { ms: w.read_u64(r)? },
        TAG_EMIT => Operation::Emit {
            event: w.read_str(r)?,
        },
        TAG_FAIL => Operation::Fail {
            reason: w.read_str(r)?,
        },
        other => return Err(DeltaError::UnknownTag(other)),
    })
}

fn push_str_wide(out: &mut Bytes, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Folds adjacent same-key operations into a single [`Operation::Compute`].
///
/// * Runs extend only while the key is unchanged and neither operation is a
///   barrier.
/// * A run that composes to the identity (`x -> x`) is dropped: the state is
///   total, so a key that reads as its old value reads as `0` whether or not it
///   was materialised.
/// * If the composed coefficients no longer fit in `i64`, the run is emitted
///   unchanged, so folding can never change a result.
#[must_use]
pub fn coalesce_deltas(deltas: &[StateDelta]) -> Vec<StateDelta> {
    let mut out: Vec<StateDelta> = Vec::with_capacity(deltas.len());
    let mut run: Option<Run> = None;

    for delta in deltas {
        let Some(r) = run.take() else {
            match delta.operation.affine() {
                Some(affine) => run = Some(Run::new(delta, affine)),
                None => out.push(delta.clone()),
            }
            continue;
        };
        match delta.operation.affine() {
            Some((key, a, b)) if r.key == key => match r.compose(delta, a, b) {
                None => {
                    // Coefficients no longer representable: keep the run on
                    // record and restart with this delta.
                    r.emit(&mut out);
                    out.push(delta.clone());
                }
                Some(next) => run = Some(next),
            },
            Some(_) => {
                r.emit(&mut out);
                run = Some(Run::new(
                    delta,
                    delta.operation.affine().expect("just matched"),
                ));
            }
            None => {
                r.emit(&mut out);
                out.push(delta.clone());
            }
        }
    }
    if let Some(r) = run.take() {
        r.emit(&mut out);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    meta: StateDelta,
    key: String,
    scale: i128,
    addend: i128,
    /// Deltas folded in. A run of one is left untouched, so coalescing can never
    /// make a chain *bigger*.
    len: usize,
}

impl Run {
    fn new(delta: &StateDelta, affine: (&str, i128, i128)) -> Self {
        let (key, scale, addend) = affine;
        Self {
            meta: delta.clone(),
            key: key.to_owned(),
            scale,
            addend,
            len: 1,
        }
    }

    /// Compose the run with the following affine map `x -> a * x + b`.
    ///
    /// `run(x) = s * x + c`, so `a * run(x) + b = (a * s) * x + (a * c + b)`.
    /// The run adopts `delta`'s metadata because once it completes, the state is
    /// exactly the state the last delta describes.
    fn compose(&self, delta: &StateDelta, a: i128, b: i128) -> Option<Self> {
        let scale = a.checked_mul(self.scale)?;
        let addend = a.checked_mul(self.addend)?.checked_add(b)?;
        if scale > i128::from(i64::MAX)
            || scale < i128::from(i64::MIN)
            || addend > i128::from(i64::MAX)
            || addend < i128::from(i64::MIN)
        {
            return None;
        }
        Some(Self {
            meta: delta.clone(),
            key: self.key.clone(),
            scale,
            addend,
            len: self.len + 1,
        })
    }

    fn emit(self, out: &mut Vec<StateDelta>) {
        if self.scale == 1 && self.addend == 0 {
            return;
        }
        if self.len == 1 {
            out.push(self.meta);
            return;
        }
        let mut delta = self.meta;
        delta.operation = Operation::Compute {
            key: self.key,
            scale: self.scale as i64,
            addend: self.addend as i64,
        };
        out.push(delta);
    }
}

/// Minimal cursor used by the codecs and the ledger framing.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DeltaError> {
        let end = self.pos.checked_add(n).ok_or(DeltaError::UnexpectedEnd)?;
        if end > self.bytes.len() {
            return Err(DeltaError::UnexpectedEnd);
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DeltaError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DeltaError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, DeltaError> {
        let s = self.take(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(s);
        Ok(u64::from_le_bytes(buf))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, DeltaError> {
        Ok(self.u64()? as i64)
    }

    fn str_wide(&mut self) -> Result<String, DeltaError> {
        let len = self.u32()? as usize;
        let s = self.take(len)?;
        String::from_utf8(s.to_vec()).map_err(|_| DeltaError::InvalidUtf8)
    }

    pub(crate) fn varint_u64(&mut self) -> Result<u64, DeltaError> {
        varint::get_u64(self)
    }

    pub(crate) fn varint_i64(&mut self) -> Result<i64, DeltaError> {
        varint::get_i64(self)
    }

    pub(crate) fn varint_str(&mut self) -> Result<String, DeltaError> {
        let len = varint::get_u64(self)? as usize;
        let s = self.take(len)?;
        String::from_utf8(s.to_vec()).map_err(|_| DeltaError::InvalidUtf8)
    }

    pub(crate) fn finish(&self) -> Result<(), DeltaError> {
        let trailing = self.bytes.len() - self.pos;
        if trailing == 0 {
            Ok(())
        } else {
            Err(DeltaError::TrailingBytes(trailing))
        }
    }

    /// Everything not yet consumed.
    pub(crate) fn rest(&self) -> Result<&'a [u8], DeltaError> {
        Ok(&self.bytes[self.pos..])
    }
}

/// LEB128 style varints, shared by the delta codecs and the ledger framing.
pub(crate) mod varint {
    use super::{Bytes, DeltaError, Reader};

    pub(crate) fn put_u64(out: &mut Bytes, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    pub(crate) fn get_u64(r: &mut Reader<'_>) -> Result<u64, DeltaError> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = r.u8()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(DeltaError::UnexpectedEnd);
            }
        }
    }

    /// Zigzag encoding so small negative numbers stay small.
    pub(crate) fn put_i64(out: &mut Bytes, value: i64) {
        put_u64(out, ((value << 1) ^ (value >> 63)) as u64);
    }

    pub(crate) fn get_i64(r: &mut Reader<'_>) -> Result<i64, DeltaError> {
        let raw = get_u64(r)?;
        Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
    }

    pub(crate) fn put_str(out: &mut Bytes, s: &str) {
        put_u64(out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(seq: u64, op: Operation) -> StateDelta {
        StateDelta::new(
            FutureId::new(1),
            StepId::new(seq),
            AttemptId::new(0),
            Version::new(seq),
            Version::new(seq + 1),
            op,
        )
    }

    fn add(seq: u64, by: i64) -> StateDelta {
        delta(
            seq,
            Operation::Add {
                key: "counter".into(),
                by,
            },
        )
    }

    fn all_ops() -> Vec<Operation> {
        vec![
            Operation::Set {
                key: "a".into(),
                value: -7,
            },
            Operation::Add {
                key: "b".into(),
                by: 3,
            },
            Operation::Compute {
                key: "c".into(),
                scale: -2,
                addend: 11,
            },
            Operation::Sleep { ms: 1_234 },
            Operation::Emit {
                event: "done".into(),
            },
            Operation::Fail {
                reason: "boom".into(),
            },
        ]
    }

    #[test]
    fn codecs_round_trip() {
        for codec in [
            &RawDeltaCodec as &dyn DeltaCodec,
            &CompactDeltaCodec as &dyn DeltaCodec,
        ] {
            for (i, op) in all_ops().into_iter().enumerate() {
                let d = delta(i as u64, op);
                let bytes = codec.encode(&d);
                assert_eq!(codec.decode(&bytes).unwrap(), d, "codec {}", codec.name());
            }
        }
    }

    #[test]
    fn compact_codec_beats_raw_codec() {
        let deltas: Vec<_> = (0..50).map(|i| add(i, 1)).collect();
        let raw = RawDeltaCodec.encoded_len(&deltas);
        let compact = CompactDeltaCodec.encoded_len(&CompactDeltaCodec.coalesce(&deltas));
        assert!(compact < raw / 10, "raw={raw} compact={compact}");
    }

    #[test]
    fn coalesce_folds_adjacent_same_key_ops() {
        let folded = coalesce_deltas(&[add(0, 1), add(1, 1), add(2, 1), add(3, -1)]);
        assert_eq!(folded.len(), 1);
        assert_eq!(
            folded[0].operation,
            Operation::Compute {
                key: "counter".into(),
                scale: 1,
                addend: 2
            }
        );
    }

    #[test]
    fn coalesce_drops_identity_runs() {
        assert!(coalesce_deltas(&[add(0, 1), add(1, -1)]).is_empty());
    }

    #[test]
    fn coalesce_respects_barriers() {
        let deltas = vec![add(0, 1), delta(1, Operation::Sleep { ms: 5 }), add(2, 1)];
        assert_eq!(coalesce_deltas(&deltas).len(), 3);
    }

    #[test]
    fn coalesce_respects_key_changes() {
        let deltas = vec![
            add(0, 1),
            delta(
                1,
                Operation::Add {
                    key: "other".into(),
                    by: 1,
                },
            ),
            add(2, 1),
        ];
        assert_eq!(coalesce_deltas(&deltas).len(), 3);
    }

    #[test]
    fn coalesce_folds_set_then_add() {
        let deltas = vec![
            delta(
                0,
                Operation::Set {
                    key: "k".into(),
                    value: 10,
                },
            ),
            delta(
                1,
                Operation::Add {
                    key: "k".into(),
                    by: 5,
                },
            ),
        ];
        let folded = coalesce_deltas(&deltas);
        assert_eq!(
            folded[0].operation,
            Operation::Compute {
                key: "k".into(),
                scale: 0,
                addend: 15
            }
        );
    }

    #[test]
    fn coalesce_keeps_last_metadata_of_run() {
        let folded = coalesce_deltas(&[add(0, 1), add(1, 1), add(2, 1)]);
        assert_eq!(folded[0].version, Version::new(3));
        assert_eq!(folded[0].step, StepId::new(2));
    }

    #[test]
    fn a_delta_knows_whether_it_continues_its_predecessor() {
        let chain = [add(0, 1), add(1, 1), add(2, 1)];
        assert!(chain[1].follows(&chain[0]));
        assert!(chain[2].follows(&chain[1]));
        let detached = StateDelta::new(
            FutureId::new(1),
            StepId::new(9),
            AttemptId::new(0),
            Version::new(0),
            Version::new(1),
            Operation::Add {
                key: "counter".into(),
                by: 1,
            },
        );
        assert!(!detached.follows(&chain[0]), "a gap must be detectable");
    }

    #[test]
    fn a_delta_from_another_future_never_follows() {
        let mut other = add(1, 1);
        other.future = FutureId::new(2);
        assert!(!other.follows(&add(0, 1)));
    }
}
