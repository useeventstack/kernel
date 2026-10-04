//! The ledger record set.
//!
//! The ledger is the only durable artefact. It is an append-only sequence of
//! length-prefixed, checksummed frames, and *every* frame describes one step of
//! one future. There is no separate "control" log and no side structure, which
//! is what makes a single append the atomic commit point.
//!
//! ```text
//! ┌────────────┬──────────────┬───────────────────────────┐
//! │ len: u32le │ fnv: u32le   │ payload (len bytes)      │
//! └────────────┴──────────────┴───────────────────────────┘
//! ```
//!
//! The checksum exists so a process that dies mid-write leaves a *detectable*
//! torn tail rather than a silently corrupt prefix — which is the failure mode
//! recovery has to survive, and the one the byte-offset sweep tests attack.
//!
//! # The payloads
//!
//! | payload | meaning | is a future's head advanced? |
//! |---|---|---|
//! | `Opened` | the execution exists, trunk at the empty state | no |
//! | `Step` | a pure transition, `cursor -> cursor+1` | yes |
//! | `Effect` | an interaction, `cursor -> cursor+1` | yes |
//! | `Fork` | children were created at this cursor | no |
//! | `Selected` | a selection was decided | no |
//! | `Committed` | a future became the trunk's head | **yes, and this is the atomic commit point** |
//! | `Settled` | a future reached a terminal state | no |
//! | `Released` | a deferred effect was issued at commit | no |
//!
//! The version assigned to a `Step` or `Effect` is the version that record
//! *produces*. `Opened`, `Fork`, `Selected` and `Settled` do not advance any
//! state, so they carry the version they observed. That keeps versions
//! meaningful: `version` counts state transitions, and a future's head version
//! is exactly the version its last `Step`/`Effect` produced.

use std::fmt;

use crate::domain::delta::{
    varint, CompactDeltaCodec, DeltaCodec, EffectDelta, Reader, StateDelta,
};
use crate::domain::effect::{Disposition, EffectClass, EffectOp, EffectValue};
use crate::domain::ids::{AttemptId, EffectId, ExecutionId, FutureId, NodeId, PlanId};
use crate::domain::state::StateError;
use crate::domain::version::{Sequence, Version};
use crate::ports::StoreError;

/// What one durable record represents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    /// The execution was created.
    Opened { plan: PlanId },
    /// A pure state transition on a future.
    Step {
        delta: Box<StateDelta>,
        cursor: NodeId,
    },
    /// An interaction on a future, with what the runtime did about it.
    ///
    /// The observed value lives inside the delta's `disposition`, not beside it.
    /// Storing it twice would mean two encodings that could disagree, and the
    /// journal is the thing replay trusts.
    Effect {
        delta: Box<EffectDelta>,
        cursor: NodeId,
    },
    /// A fork: children created from this future at this cursor.
    Fork {
        parent: FutureId,
        cursor: NodeId,
        /// `(future id, arm index, arm root, base state)`.
        children: Vec<ForkedChild>,
    },
    /// A selection was decided among the given futures.
    Selected {
        among: Vec<FutureId>,
        scores: Vec<Option<i64>>,
        winner: FutureId,
        cursor: NodeId,
    },
    /// A future became authoritative. **The atomic commit point.**
    Committed {
        future: FutureId,
        cursor: NodeId,
        /// The trunk head after the merge.
        trunk: crate::domain::node::StateNode,
        /// The trunk version after the merge.
        trunk_version: Version,
        /// Deferred effects released by this commit, in issue order.
        release: Vec<EffectId>,
    },
    /// A future reached a terminal state.
    Settled {
        future: FutureId,
        status: crate::domain::future::FutureStatus,
    },
    /// A deferred effect was issued to the world as part of a commit.
    Released { future: FutureId, ordinal: EffectId },
}

/// One child created by a fork.
///
/// `base` is a content address, so recording a fork costs 17 bytes per child no
/// matter how large the state is. That is the O(1)-branch property expressed in
/// the log format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForkedChild {
    pub future: FutureId,
    pub arm: usize,
    /// Cursor the child starts at.
    pub root: NodeId,
    /// Cursor at which the child is finished. Half-open: the child runs
    /// `[root, end)`.
    pub end: NodeId,
    /// Content address of the state the child forked from. Recording a fork
    /// therefore costs a fixed number of bytes per child, whatever the state
    /// size.
    pub base: crate::domain::node::StateNode,
}

impl Payload {
    fn tag(&self) -> u8 {
        match self {
            Payload::Opened { .. } => 0,
            Payload::Step { .. } => 1,
            Payload::Effect { .. } => 2,
            Payload::Fork { .. } => 3,
            Payload::Selected { .. } => 4,
            Payload::Committed { .. } => 5,
            Payload::Settled { .. } => 6,
            Payload::Released { .. } => 7,
        }
    }

    /// The future this record is about, for indexing and for replay dispatch.
    #[must_use]
    pub fn future(&self) -> Option<FutureId> {
        match self {
            Payload::Opened { .. } => None,
            Payload::Step { delta, .. } => Some(delta.future),
            Payload::Effect { delta, .. } => Some(delta.future),
            Payload::Fork { parent, .. } => Some(*parent),
            Payload::Selected { winner, .. } => Some(*winner),
            Payload::Committed { future, .. } => Some(*future),
            Payload::Settled { future, .. } | Payload::Released { future, .. } => Some(*future),
        }
    }

    /// Every `(future, cursor)` this record leaves behind.
    ///
    /// Not only `Step` and `Effect`. A `Fork` moves the parent's cursor, a
    /// `Selected` moves the winner's, and a `Committed` moves *two*: the winner's
    /// and the trunk's, because the commit is the moment the winner's work becomes
    /// the trunk's. Leaving any of those out means a future that has done no state
    /// work but has forked, selected or committed has no durable cursor at all, and
    /// a rollback resumes it from wherever the map happened to say — which for a
    /// committed trunk is *before* its own commit, so it re-commits and the run
    /// fails on a future that is already authoritative.
    #[must_use]
    pub fn cursors_moved(&self) -> Vec<(crate::domain::ids::FutureId, crate::domain::ids::NodeId)> {
        use crate::domain::ids::TRUNK;
        match self {
            Payload::Step { delta, cursor } => vec![(delta.future, *cursor)],
            Payload::Effect { delta, cursor } => vec![(delta.future, *cursor)],
            Payload::Fork { parent, cursor, .. } => vec![(*parent, *cursor)],
            Payload::Selected { winner, cursor, .. } => vec![(*winner, *cursor)],
            Payload::Committed { future, cursor, .. } => {
                vec![(*future, *cursor), (TRUNK, *cursor)]
            }
            Payload::Opened { .. } | Payload::Settled { .. } | Payload::Released { .. } => {
                Vec::new()
            }
        }
    }

    /// Whether this record advances its future's state head.
    #[must_use]
    pub fn advances_head(&self) -> bool {
        matches!(
            self,
            Payload::Step { .. } | Payload::Effect { .. } | Payload::Committed { .. }
        )
    }

    /// Short human readable summary, used by `inspect` and by test failures.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Payload::Opened { plan } => format!("opened plan {plan}"),
            Payload::Step { delta, cursor } => {
                format!(
                    "{} step {cursor}: {}",
                    delta.future,
                    delta.operation.describe()
                )
            }
            Payload::Effect { delta, cursor } => format!(
                "{} effect {cursor}: {} {} -> {}",
                delta.future, delta.class, delta.op, delta.disposition
            ),
            Payload::Fork {
                parent,
                cursor,
                children,
            } => format!(
                "{parent} fork at {cursor} -> {}",
                children
                    .iter()
                    .map(|c| format!("{}(arm {})@{}<{}", c.future, c.arm, c.root, c.base))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Payload::Selected {
                among,
                scores,
                winner,
                cursor,
            } => format!(
                "selected {winner} at {cursor} from [{}]",
                among
                    .iter()
                    .zip(scores)
                    .map(|(f, s)| format!(
                        "{f}={}",
                        s.map_or_else(|| "-".into(), |v| v.to_string())
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Payload::Committed {
                future,
                trunk,
                trunk_version,
                release,
                ..
            } => format!(
                "committed {future} -> trunk {trunk} at {trunk_version}, releasing {} effect(s)",
                release.len()
            ),
            Payload::Settled { future, status } => format!("{future} settled {status}"),
            Payload::Released { future, ordinal } => format!("{future} released effect {ordinal}"),
        }
    }
}

/// One durable ledger record. Self-describing: everything recovery needs is in
/// the record, not in a side structure that could disagree with the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerRecord {
    pub execution: ExecutionId,
    pub version: Version,
    pub sequence: Sequence,
    pub attempt: AttemptId,
    pub timestamp_us: u64,
    pub payload: Payload,
}

impl LedgerRecord {
    #[must_use]
    pub fn new(
        execution: ExecutionId,
        version: Version,
        sequence: Sequence,
        attempt: AttemptId,
        timestamp_us: u64,
        payload: Payload,
    ) -> Self {
        Self {
            execution,
            version,
            sequence,
            attempt,
            timestamp_us,
            payload,
        }
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(48);
        out.push(self.payload.tag());
        varint::put_u64(&mut out, self.execution.value());
        varint::put_u64(&mut out, self.version.value());
        varint::put_u64(&mut out, self.sequence.value());
        varint::put_u64(&mut out, self.attempt.value());
        varint::put_u64(&mut out, self.timestamp_us);
        match &self.payload {
            Payload::Opened { plan } => varint::put_u64(&mut out, plan.value()),
            Payload::Step { delta, cursor } => {
                varint::put_u64(&mut out, cursor.value());
                out.extend_from_slice(&CompactDeltaCodec.encode(delta));
            }
            Payload::Effect { delta, cursor } => {
                varint::put_u64(&mut out, cursor.value());
                encode_effect_delta(&mut out, delta);
            }
            Payload::Fork {
                parent,
                cursor,
                children,
            } => {
                varint::put_u64(&mut out, parent.value());
                varint::put_u64(&mut out, cursor.value());
                varint::put_u64(&mut out, children.len() as u64);
                for c in children {
                    varint::put_u64(&mut out, c.future.value());
                    varint::put_u64(&mut out, c.arm as u64);
                    varint::put_u64(&mut out, c.root.value());
                    varint::put_u64(&mut out, c.end.value());
                    varint::put_u64(&mut out, c.base.value());
                }
            }
            Payload::Selected {
                among,
                scores,
                winner,
                cursor,
            } => {
                varint::put_u64(&mut out, cursor.value());
                varint::put_u64(&mut out, winner.value());
                varint::put_u64(&mut out, among.len() as u64);
                for (f, s) in among.iter().zip(scores) {
                    varint::put_u64(&mut out, f.value());
                    match s {
                        None => out.push(0),
                        Some(v) => {
                            out.push(1);
                            varint::put_i64(&mut out, *v);
                        }
                    }
                }
            }
            Payload::Committed {
                future,
                cursor,
                trunk,
                trunk_version,
                release,
            } => {
                varint::put_u64(&mut out, future.value());
                varint::put_u64(&mut out, cursor.value());
                varint::put_u64(&mut out, trunk.value());
                varint::put_u64(&mut out, trunk_version.value());
                varint::put_u64(&mut out, release.len() as u64);
                for r in release {
                    varint::put_u64(&mut out, r.value());
                }
            }
            Payload::Settled { future, status } => {
                varint::put_u64(&mut out, future.value());
                out.push(status_code(*status));
            }
            Payload::Released { future, ordinal } => {
                varint::put_u64(&mut out, future.value());
                varint::put_u64(&mut out, ordinal.value());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<LedgerRecord, StoreError> {
        let mut r = Reader::new(bytes);
        let tag = r.u8().map_err(codec)?;
        let execution = ExecutionId::new(r.varint_u64().map_err(codec)?);
        let version = Version::new(r.varint_u64().map_err(codec)?);
        let sequence = Sequence::new(r.varint_u64().map_err(codec)?);
        let attempt = AttemptId::new(r.varint_u64().map_err(codec)?);
        let timestamp_us = r.varint_u64().map_err(codec)?;
        let payload = match tag {
            0 => Payload::Opened {
                plan: PlanId::new(r.varint_u64().map_err(codec)?),
            },
            1 => {
                let cursor = NodeId::new(r.varint_u64().map_err(codec)?);
                let delta = CompactDeltaCodec
                    .decode(r.rest().map_err(codec)?)
                    .map_err(codec)?;
                Payload::Step {
                    delta: Box::new(delta),
                    cursor,
                }
            }
            2 => {
                let cursor = NodeId::new(r.varint_u64().map_err(codec)?);
                let delta = decode_effect_delta(&mut r)?;
                Payload::Effect {
                    delta: Box::new(delta),
                    cursor,
                }
            }
            3 => {
                let parent = FutureId::new(r.varint_u64().map_err(codec)?);
                let cursor = NodeId::new(r.varint_u64().map_err(codec)?);
                let n = r.varint_u64().map_err(codec)? as usize;
                if n > 1 << 20 {
                    return Err(StoreError::Corrupt {
                        reason: format!("fork with {n} children is implausible"),
                    });
                }
                let mut children = Vec::with_capacity(n);
                for _ in 0..n {
                    children.push(ForkedChild {
                        future: FutureId::new(r.varint_u64().map_err(codec)?),
                        arm: r.varint_u64().map_err(codec)? as usize,
                        root: NodeId::new(r.varint_u64().map_err(codec)?),
                        end: NodeId::new(r.varint_u64().map_err(codec)?),
                        base: crate::domain::node::StateNode::new(r.varint_u64().map_err(codec)?),
                    });
                }
                Payload::Fork {
                    parent,
                    cursor,
                    children,
                }
            }
            4 => {
                let cursor = NodeId::new(r.varint_u64().map_err(codec)?);
                let winner = FutureId::new(r.varint_u64().map_err(codec)?);
                let n = r.varint_u64().map_err(codec)? as usize;
                if n > 1 << 20 {
                    return Err(StoreError::Corrupt {
                        reason: format!("selection among {n} futures is implausible"),
                    });
                }
                let mut among = Vec::with_capacity(n);
                let mut scores = Vec::with_capacity(n);
                for _ in 0..n {
                    among.push(FutureId::new(r.varint_u64().map_err(codec)?));
                    scores.push(match r.u8().map_err(codec)? {
                        0 => None,
                        1 => Some(r.varint_i64().map_err(codec)?),
                        other => {
                            return Err(StoreError::Corrupt {
                                reason: format!("unknown score tag {other}"),
                            })
                        }
                    });
                }
                Payload::Selected {
                    among,
                    scores,
                    winner,
                    cursor,
                }
            }
            5 => {
                let future = FutureId::new(r.varint_u64().map_err(codec)?);
                let cursor = NodeId::new(r.varint_u64().map_err(codec)?);
                let trunk = crate::domain::node::StateNode::new(r.varint_u64().map_err(codec)?);
                let trunk_version = Version::new(r.varint_u64().map_err(codec)?);
                let n = r.varint_u64().map_err(codec)? as usize;
                if n > 1 << 20 {
                    return Err(StoreError::Corrupt {
                        reason: format!("commit releasing {n} effects is implausible"),
                    });
                }
                let mut release = Vec::with_capacity(n);
                for _ in 0..n {
                    release.push(EffectId::new(r.varint_u64().map_err(codec)?));
                }
                Payload::Committed {
                    future,
                    cursor,
                    trunk,
                    trunk_version,
                    release,
                }
            }
            6 => Payload::Settled {
                future: FutureId::new(r.varint_u64().map_err(codec)?),
                status: status_from_code(r.u8().map_err(codec)?).ok_or_else(|| {
                    StoreError::Corrupt {
                        reason: "unknown future status".to_owned(),
                    }
                })?,
            },
            7 => Payload::Released {
                future: FutureId::new(r.varint_u64().map_err(codec)?),
                ordinal: EffectId::new(r.varint_u64().map_err(codec)?),
            },
            other => {
                return Err(StoreError::Corrupt {
                    reason: format!("unknown ledger record tag {other}"),
                })
            }
        };
        Ok(LedgerRecord {
            execution,
            version,
            sequence,
            attempt,
            timestamp_us,
            payload,
        })
    }
}

fn codec(e: crate::domain::delta::DeltaError) -> StoreError {
    StoreError::Codec(e.to_string())
}

/// The status wire code, taken from the domain table so the two cannot drift.
fn status_code(s: crate::domain::future::FutureStatus) -> u8 {
    s.code()
}

/// The status for a wire code, likewise.
fn status_from_code(code: u8) -> Option<crate::domain::future::FutureStatus> {
    crate::domain::future::FutureStatus::from_code(code)
}

fn encode_effect_delta(out: &mut Vec<u8>, d: &EffectDelta) {
    varint::put_u64(out, d.future.value());
    varint::put_u64(out, d.ordinal.value());
    varint::put_u64(out, d.attempt.value());
    varint::put_u64(out, d.parent.value());
    varint::put_u64(out, d.version.value());
    out.push(class_code(d.class));
    varint::put_str(out, &d.op.name);
    varint::put_i64(out, d.op.args);
    match &d.disposition {
        Disposition::Performed { result } => {
            out.push(0);
            encode_value(out, result);
        }
        Disposition::Replayed { result } => {
            out.push(1);
            encode_value(out, result);
        }
        Disposition::Deferred => out.push(2),
    }
    put_opt_str(out, d.reads_into.as_deref());
    put_opt_str(out, d.writes.as_deref());
    match &d.compensation {
        None => out.push(0),
        Some(c) => {
            out.push(1);
            varint::put_str(out, &c.name);
            varint::put_i64(out, c.args);
        }
    }
}

fn decode_effect_delta(r: &mut Reader<'_>) -> Result<EffectDelta, StoreError> {
    let future = FutureId::new(r.varint_u64().map_err(codec)?);
    let ordinal = EffectId::new(r.varint_u64().map_err(codec)?);
    let attempt = AttemptId::new(r.varint_u64().map_err(codec)?);
    let parent = Version::new(r.varint_u64().map_err(codec)?);
    let version = Version::new(r.varint_u64().map_err(codec)?);
    let class = class_from_code(r.u8().map_err(codec)?).ok_or_else(|| StoreError::Corrupt {
        reason: "unknown effect class".to_owned(),
    })?;
    let op = EffectOp {
        name: r.varint_str().map_err(codec)?,
        args: r.varint_i64().map_err(codec)?,
    };
    let disposition_kind = r.u8().map_err(codec)?;
    let value = if disposition_kind == 2 {
        None
    } else {
        Some(decode_value(r)?)
    };
    let reads_into = get_opt_str(r)?;
    let writes = get_opt_str(r)?;
    let compensation = match r.u8().map_err(codec)? {
        0 => None,
        1 => Some(EffectOp {
            name: r.varint_str().map_err(codec)?,
            args: r.varint_i64().map_err(codec)?,
        }),
        other => {
            return Err(StoreError::Corrupt {
                reason: format!("unknown compensation tag {other}"),
            })
        }
    };
    let disposition = match (disposition_kind, value) {
        (0, Some(v)) => Disposition::Performed { result: v },
        (1, Some(v)) => Disposition::Replayed { result: v },
        (2, _) => Disposition::Deferred,
        (other, _) => {
            return Err(StoreError::Corrupt {
                reason: format!("unknown disposition tag {other}"),
            })
        }
    };
    Ok(EffectDelta {
        future,
        ordinal,
        attempt,
        parent,
        version,
        class,
        op,
        reads_into,
        writes,
        compensation,
        disposition,
    })
}

fn encode_value(out: &mut Vec<u8>, v: &EffectValue) {
    match v {
        EffectValue::Int(i) => {
            out.push(1);
            varint::put_i64(out, *i);
        }
        EffectValue::Text(t) => {
            out.push(2);
            varint::put_str(out, t);
        }
        EffectValue::Unit => out.push(3),
    }
}

fn decode_value(r: &mut Reader<'_>) -> Result<EffectValue, StoreError> {
    match r.u8().map_err(codec)? {
        1 => Ok(EffectValue::Int(r.varint_i64().map_err(codec)?)),
        2 => Ok(EffectValue::Text(r.varint_str().map_err(codec)?)),
        3 => Ok(EffectValue::Unit),
        other => Err(StoreError::Corrupt {
            reason: format!("unknown effect value tag {other}"),
        }),
    }
}

fn put_opt_str(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            varint::put_str(out, v);
        }
    }
}

fn get_opt_str(r: &mut Reader<'_>) -> Result<Option<String>, StoreError> {
    match r.u8().map_err(codec)? {
        0 => Ok(None),
        1 => Ok(Some(r.varint_str().map_err(codec)?)),
        other => Err(StoreError::Corrupt {
            reason: format!("unknown optional-string tag {other}"),
        }),
    }
}

/// The class wire code, taken from the domain table so the two cannot drift.
fn class_code(c: EffectClass) -> u8 {
    c.code()
}

fn class_from_code(code: u8) -> Option<EffectClass> {
    EffectClass::from_code(code)
}

impl From<StateError> for StoreError {
    fn from(e: StateError) -> Self {
        StoreError::Codec(e.to_string())
    }
}

impl fmt::Display for LedgerRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "seq={} ver={} attempt={} t={}us {}",
            self.sequence.value(),
            self.version.value(),
            self.attempt.value(),
            self.timestamp_us,
            self.payload.summary()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::effect::EffectKey;
    use crate::domain::ids::StepId;
    use crate::domain::ids::TRUNK;
    use crate::domain::node::StateNode;
    use crate::domain::state::State;
    use crate::domain::Operation;

    fn record(payload: Payload) -> LedgerRecord {
        LedgerRecord::new(
            ExecutionId::new(1),
            Version::new(4),
            Sequence::new(3),
            AttemptId::new(1),
            4242,
            payload,
        )
    }

    fn round_trip(payload: Payload) {
        let r = record(payload);
        let decoded = LedgerRecord::decode(&r.encode()).unwrap();
        assert_eq!(decoded, r);
        assert!(!r.to_string().is_empty());
    }

    #[test]
    fn every_payload_round_trips() {
        round_trip(Payload::Opened {
            plan: PlanId::new(9),
        });
        round_trip(Payload::Step {
            delta: Box::new(StateDelta::new(
                FutureId::new(2),
                StepId::new(1),
                AttemptId::new(0),
                Version::new(1),
                Version::new(2),
                Operation::Add {
                    key: "counter".into(),
                    by: 3,
                },
            )),
            cursor: NodeId::new(2),
        });
        round_trip(Payload::Effect {
            delta: Box::new(
                EffectDelta::new(
                    FutureId::new(2),
                    EffectId::new(0),
                    AttemptId::new(0),
                    Version::new(2),
                    Version::new(3),
                    EffectClass::Irreversible,
                    EffectOp::new("charge", 42),
                    Disposition::Deferred,
                )
                .writing("charged")
                .with_compensation(EffectOp::new("refund", 42)),
            ),
            cursor: NodeId::new(3),
        });
        round_trip(Payload::Effect {
            delta: Box::new(
                EffectDelta::new(
                    FutureId::new(2),
                    EffectId::new(1),
                    AttemptId::new(0),
                    Version::new(3),
                    Version::new(4),
                    EffectClass::Read,
                    EffectOp::new("quote", 7),
                    Disposition::Performed {
                        result: EffectValue::Int(14),
                    },
                )
                .reading_into("q"),
            ),
            cursor: NodeId::new(4),
        });
        round_trip(Payload::Fork {
            parent: TRUNK,
            cursor: NodeId::new(1),
            children: vec![
                ForkedChild {
                    future: FutureId::new(1),
                    arm: 0,
                    root: NodeId::new(2),
                    end: NodeId::new(3),
                    base: StateNode::new(1234),
                },
                ForkedChild {
                    future: FutureId::new(2),
                    arm: 1,
                    root: NodeId::new(5),
                    end: NodeId::new(6),
                    base: StateNode::new(1234),
                },
            ],
        });
        round_trip(Payload::Selected {
            among: vec![FutureId::new(1), FutureId::new(2)],
            scores: vec![Some(10), None],
            winner: FutureId::new(1),
            cursor: NodeId::new(6),
        });
        round_trip(Payload::Committed {
            future: FutureId::new(1),
            cursor: NodeId::new(7),
            trunk: StateNode::new(999),
            trunk_version: Version::new(12),
            release: vec![EffectId::new(0), EffectId::new(2)],
        });
        for status in crate::domain::future::FutureStatus::ALL {
            round_trip(Payload::Settled {
                future: FutureId::new(3),
                status,
            });
        }
        round_trip(Payload::Released {
            future: FutureId::new(1),
            ordinal: EffectId::new(0),
        });
    }

    #[test]
    fn every_status_and_class_survives_the_wire() {
        for s in crate::domain::future::FutureStatus::ALL {
            assert_eq!(status_from_code(status_code(s)), Some(s), "{s}");
        }
        for c in crate::domain::EffectClass::ALL {
            assert_eq!(class_from_code(class_code(c)), Some(c), "{c}");
        }
    }

    #[test]
    fn a_fork_costs_the_same_however_big_the_state_it_forks_from() {
        // The point of content addressing: the fork record names a state, it does
        // not contain one. Two states of very different sizes produce the same
        // record, which is what makes N-way evaluation affordable.
        let mut big = State::new();
        for i in 0..2_000 {
            big.set(&format!("key-{i}"), i as i64);
        }
        let mut small = State::new();
        small.set("k", 1);
        let fork_for = |base: StateNode| Payload::Fork {
            parent: TRUNK,
            cursor: NodeId::new(1),
            children: vec![ForkedChild {
                future: FutureId::new(1),
                arm: 0,
                root: NodeId::new(2),
                end: NodeId::new(3),
                base,
            }],
        };
        // Addresses with the same encoded width (both fit in two varint bytes)
        // must produce byte-identical records.
        let big_node = StateNode::new(big.content_hash() & 0x3fff | 0x40);
        let small_node = StateNode::new(small.content_hash() & 0x3fff | 0x40);
        assert_eq!(
            record(fork_for(big_node)).encode().len(),
            record(fork_for(small_node)).encode().len()
        );
        // And a fork record is nowhere near the size of the state it points at.
        let encoded = record(fork_for(big_node)).encode();
        assert!(
            encoded.len() < 40,
            "a fork record must not scale with the state: {} bytes for a state of {} entries",
            encoded.len(),
            big.len()
        );
    }

    #[test]
    fn an_unknown_tag_is_corruption() {
        let mut bytes = record(Payload::Opened {
            plan: PlanId::new(1),
        })
        .encode();
        bytes[0] = 99;
        assert!(matches!(
            LedgerRecord::decode(&bytes),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn an_implausible_fork_width_is_rejected() {
        // A well-formed envelope, then a fork claiming two billion children. A
        // decoder that trusted the width would try to allocate two billion
        // children; the bound is what makes decoding an untrusted ledger safe.
        let mut bytes = Vec::new();
        bytes.push(3); // fork
                       // The envelope: execution, version, sequence, attempt, timestamp.
        for _ in 0..5 {
            varint::put_u64(&mut bytes, 1);
        }
        // The payload: parent, cursor, child count.
        varint::put_u64(&mut bytes, TRUNK.value());
        varint::put_u64(&mut bytes, 0);
        varint::put_u64(&mut bytes, 1 << 30);
        assert!(matches!(
            LedgerRecord::decode(&bytes),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn an_effect_key_names_the_execution_the_arm_path_and_the_ordinal() {
        // The string form is what ends up in a target's dedup table, so it has to
        // read the same way the identity does: execution first, then the arm path,
        // then the ordinal. A key that did not include the execution would collide
        // across two runs of the same plan against a shared target.
        let k = EffectKey::new(ExecutionId::new(1), vec![0, 1], EffectId::new(1));
        assert_eq!(k.as_string(), "e1/0.1.e1");
        assert_eq!(k.path(), &[0, 1]);
    }
}
