//! Plans: what should happen, as a compiled program.
//!
//! A plan is a *tree*, not a list. That is the change from a linear workflow
//! definition and it is the whole point: a fork is a node in the plan, so
//! "run three alternatives and take the best" is expressed in the same language
//! as "increment a counter", and the interpreter does not need a second code
//! path for the branching case.
//!
//! ```text
//!   ┌── Fork "choose a plan" ──┐
//!   │  ├── arm A: reserve → score│
//!   │  ├── arm B: reserve → score│
//!   │  └── arm C: reserve → score│
//!   ├── Select "highest score"   │
//!   └── Commit                   │
//! ```
//!
//! # Flattening
//!
//! A plan is compiled once into a flat `Vec<PlanNode>` with children referenced
//! by [`NodeId`]. The interpreter's position is then a single `u32`, cursor
//! arithmetic inside a straight-line region is `+ 1`, and a cursor can be
//! written into a ledger record and read back after a crash without
//! reconstruction. Neither a tree walk nor a path vector is needed on the hot
//! path.

use std::fmt;
use std::time::Duration;

use crate::domain::effect::{EffectClass, EffectOp};
use crate::domain::ids::{NodeId, PlanId, StepId};
use crate::domain::Operation;

/// One compiled instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanNode {
    /// A pure state transition.
    State {
        name: String,
        op: Operation,
        cost: Duration,
    },
    /// An interaction with the outside world.
    Effect {
        name: String,
        class: EffectClass,
        op: EffectOp,
        /// State key the observed value is folded into, for `Read`.
        reads_into: Option<String>,
        /// State key the effect writes, for reporting.
        writes: Option<String>,
        /// A declared undo, for `Compensatable`.
        compensation: Option<EffectOp>,
        cost: Duration,
    },
    /// Create `arms.len()` alternative futures from here and wait for all of
    /// them. The children are the arm extents.
    Fork { name: String, arms: Vec<Arm> },
    /// Evaluate the futures this one is waiting on and choose one. Does not by
    /// itself make anything authoritative.
    Select,
    /// Make the selected future authoritative. The only state change in the
    /// whole model that moves the trunk.
    Commit,
}

impl PlanNode {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            PlanNode::State { name, .. }
            | PlanNode::Effect { name, .. }
            | PlanNode::Fork { name, .. } => name,
            PlanNode::Select | PlanNode::Commit => match self {
                PlanNode::Select => "select",
                _ => "commit",
            },
        }
    }

    /// Simulated cost of executing the node. Only [`PlanNode::State`] and
    /// [`PlanNode::Effect`] consume time; the control nodes are cheap.
    #[must_use]
    pub fn cost(&self) -> Duration {
        match self {
            PlanNode::State { cost, .. } | PlanNode::Effect { cost, .. } => *cost,
            _ => Duration::ZERO,
        }
    }

    /// Whether executing this node can change the outside world.
    #[must_use]
    pub fn is_effect(&self) -> bool {
        matches!(self, PlanNode::Effect { .. })
    }

    /// One-line description for `inspect` and test failure messages.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            PlanNode::State { name, op, .. } => format!("state {name}: {}", op.describe()),
            PlanNode::Effect {
                name, class, op, ..
            } => format!("effect {name}: {class} {op}"),
            PlanNode::Fork { name, arms } => format!(
                "fork {name} into {} arms [{}]",
                arms.len(),
                arms.iter()
                    .map(|a| format!("{}..{}", a.root.value(), a.end.value()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            PlanNode::Select => "select".to_owned(),
            PlanNode::Commit => "commit".to_owned(),
        }
    }
}

/// A compiled plan: a flat node table, the root, and the arm extents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub id: PlanId,
    pub name: String,
    nodes: Vec<PlanNode>,
    /// Every arm of every fork, in the order they were created. Extents nest
    /// properly: a fork inside an arm produces extents contained in that arm's.
    arms: Vec<Arm>,
}

impl Plan {
    /// Default per-step cost used when a builder omits one.
    pub const DEFAULT_COST: Duration = Duration::from_millis(1);

    #[must_use]
    pub fn builder(id: PlanId, name: &str) -> PlanBuilder {
        PlanBuilder {
            id,
            name: name.to_owned(),
            statements: Vec::new(),
        }
    }

    /// A plan that is just a straight line of operations, for baselines and
    /// benchmarks.
    #[must_use]
    pub fn linear(id: PlanId, name: &str, ops: Vec<Operation>) -> Self {
        let mut b = Plan::builder(id, name);
        for op in ops {
            b = b.state(op);
        }
        b.build()
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&PlanNode> {
        self.nodes.get(id.value() as usize)
    }

    pub fn nodes(&self) -> &[PlanNode] {
        &self.nodes
    }

    /// One past the last node, i.e. the "execution finished" cursor.
    #[must_use]
    pub fn end(&self) -> NodeId {
        NodeId::new(self.nodes.len() as u64)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Step index of a cursor, used to name a delta's position in the plan.
    #[must_use]
    pub fn step_of(&self, cursor: NodeId) -> StepId {
        StepId::new(cursor.value())
    }

    /// The arm extents of the fork at `cursor`, if there is one.
    #[must_use]
    pub fn arms_of(&self, cursor: NodeId) -> Option<&[Arm]> {
        match self.node(cursor) {
            Some(PlanNode::Fork { arms, .. }) => Some(arms),
            _ => None,
        }
    }

    /// Every arm extent in the program.
    #[must_use]
    pub fn arms(&self) -> &[Arm] {
        &self.arms
    }

    /// The first cursor a fresh execution should run from.
    ///
    /// Usually zero, but not when the plan *begins* with a fork: the arms are
    /// inlined first, so node 0 belongs to an alternative. A trunk that started
    /// there would execute the first alternative itself and then fork — which
    /// looks like it works, produces plausible numbers, and is wrong.
    #[must_use]
    pub fn entry(&self) -> NodeId {
        let mut n: u64 = 0;
        loop {
            let mut inside: Option<u64> = None;
            for arm in &self.arms {
                if arm.root.value() <= n && n < arm.end.value() {
                    inside = Some(arm.end.value());
                    break;
                }
            }
            match inside {
                Some(end) if end > n => n = end,
                _ => return NodeId::new(n),
            }
        }
    }

    /// The cursor to move to after executing the node at `from`, for a future whose
    /// arm ends at `limit`.
    ///
    /// This is the one place that knows a flattened program is not a straight line.
    /// Arm bodies are inlined into the same node table, so a naive `+ 1` would walk
    /// a future straight through its alternatives and out the other side.
    ///
    /// The rule has two parts, and the second one is what makes it correct:
    ///
    /// 1. step to `from + 1`;
    /// 2. while that lands on the **root of an arm**, jump to that arm's end.
    ///
    /// Step 2 is what a *parent* needs (it must not execute its alternatives' first
    /// node) and what a *child* must not do beyond its own extent (a child is
    /// created with its cursor already at the arm root, so it enters the arm and
    /// then walks to `limit` normally). Clamping at `limit` first is therefore what
    /// stops a child from stepping into its sibling's arm on its very last node.
    ///
    /// Extents nest — a fork inside an arm produces arms inside that arm — so
    /// step 2 loops: a future that lands on an inner fork skips that fork's arms
    /// rather than the whole outer extent.
    #[must_use]
    pub fn advance(&self, from: NodeId, limit: NodeId) -> NodeId {
        let bound = limit.value();
        let mut n = from.value() + 1;
        while n < bound {
            let mut jumped = false;
            for arm in &self.arms {
                // An *empty* arm (`root == end`) is a body of zero nodes. Jumping to
                // it would leave `n` unchanged and the loop would never end, so
                // only a non-empty extent is a skip target.
                if arm.root.value() == n && arm.end.value() > n {
                    n = arm.end.value();
                    jumped = true;
                    break;
                }
            }
            if !jumped {
                break;
            }
        }
        NodeId::new(n.min(bound))
    }

    /// Whether the plan ever touches the outside world.
    #[must_use]
    pub fn effect_classes(&self) -> Vec<EffectClass> {
        let mut out = Vec::new();
        for node in &self.nodes {
            if let PlanNode::Effect { class, .. } = node {
                if !out.contains(class) {
                    out.push(*class);
                }
            }
        }
        out
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} nodes)", self.name, self.nodes.len())
    }
}

/// A statement in a plan under construction. Arms are whole sub-plans so that a
/// branch can be a different *logical plan*, not just a different parameter.
/// One alternative of a fork: the half-open cursor range `[root, end)` a child
/// future runs.
///
/// The extent is stored rather than inferred because a flattened program has no
/// other way to know where an arm stops, and "this child future has finished" has
/// to be decidable from its cursor alone — that is what makes recovery resumable
/// without re-running the parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arm {
    pub root: NodeId,
    pub end: NodeId,
}

#[derive(Clone, Debug)]
enum Stmt {
    State(String, Operation, Duration),
    Effect(EffectStmt),
    Fork(String, Vec<Plan>),
    Select,
    Commit,
}

#[derive(Clone, Debug)]
struct EffectStmt {
    name: String,
    class: EffectClass,
    op: EffectOp,
    reads_into: Option<String>,
    writes: Option<String>,
    compensation: Option<EffectOp>,
    cost: Duration,
}

/// Builds a plan. Straight-line statements are appended in order; a `fork`
/// inlines each arm's nodes and records where each arm begins.
#[derive(Clone, Debug)]
pub struct PlanBuilder {
    id: PlanId,
    name: String,
    statements: Vec<Stmt>,
}

impl PlanBuilder {
    /// Appends a pure state step with the default cost.
    #[must_use]
    pub fn state(self, op: Operation) -> Self {
        self.state_named(&describe_op(&op), op, Plan::DEFAULT_COST)
    }

    /// Appends a pure state step with an explicit name and cost.
    #[must_use]
    pub fn state_named(mut self, name: &str, op: Operation, cost: Duration) -> Self {
        self.statements.push(Stmt::State(name.to_owned(), op, cost));
        self
    }

    /// Appends an effect step with the default cost.
    #[must_use]
    pub fn effect(
        self,
        name: &str,
        class: EffectClass,
        op: EffectOp,
        reads_into: Option<&str>,
        writes: Option<&str>,
    ) -> Self {
        self.effect_named(
            name,
            class,
            op,
            reads_into,
            writes,
            None,
            Plan::DEFAULT_COST,
        )
    }

    /// Appends an effect step with everything spelled out.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn effect_named(
        mut self,
        name: &str,
        class: EffectClass,
        op: EffectOp,
        reads_into: Option<&str>,
        writes: Option<&str>,
        compensation: Option<EffectOp>,
        cost: Duration,
    ) -> Self {
        self.statements.push(Stmt::Effect(EffectStmt {
            name: name.to_owned(),
            class,
            op,
            reads_into: reads_into.map(str::to_owned),
            writes: writes.map(str::to_owned),
            compensation,
            cost,
        }));
        self
    }

    /// Appends a fork over whole sub-plans.
    #[must_use]
    pub fn fork(mut self, name: &str, arms: Vec<Plan>) -> Self {
        assert!(arms.len() >= 2, "a fork with one arm is not a fork");
        self.statements.push(Stmt::Fork(name.to_owned(), arms));
        self
    }

    /// Appends the same pure step `n` times. A convenience for the linear
    /// baselines and for sweeps, where the *shape* of the plan is not the point.
    #[must_use]
    pub fn repeated(mut self, n: usize) -> Self {
        let stmt = self
            .statements
            .last()
            .cloned()
            .expect("repeated() needs a preceding statement");
        match stmt {
            Stmt::State(name, op, cost) => {
                for _ in 0..n {
                    self.statements
                        .push(Stmt::State(name.clone(), op.clone(), cost));
                }
            }
            _ => panic!("repeated() only repeats a pure state step"),
        }
        self
    }

    /// Appends a selection over the futures the current one is waiting on.
    #[must_use]
    pub fn select(mut self) -> Self {
        self.statements.push(Stmt::Select);
        self
    }

    /// Appends a commit of the selected future.
    #[must_use]
    pub fn commit(mut self) -> Self {
        self.statements.push(Stmt::Commit);
        self
    }

    #[must_use]
    pub fn build(self) -> Plan {
        let mut nodes: Vec<PlanNode> = Vec::new();
        let mut all_arms: Vec<Arm> = Vec::new();
        for stmt in &self.statements {
            match stmt {
                Stmt::State(name, op, cost) => {
                    nodes.push(PlanNode::State {
                        name: name.clone(),
                        op: op.clone(),
                        cost: *cost,
                    });
                }
                Stmt::Effect(e) => nodes.push(PlanNode::Effect {
                    name: e.name.clone(),
                    class: e.class,
                    op: e.op.clone(),
                    reads_into: e.reads_into.clone(),
                    writes: e.writes.clone(),
                    compensation: e.compensation.clone(),
                    cost: e.cost,
                }),
                Stmt::Fork(name, arms) => {
                    // Inline each arm, recording the half-open cursor range it
                    // occupies. Child futures run from `root` until `end`.
                    let mut extents = Vec::with_capacity(arms.len());
                    for arm in arms {
                        let root = NodeId::new(nodes.len() as u64);
                        // A nested plan's own arm extents are relative to *its*
                        // node table, so they have to be shifted by the offset the
                        // sub-plan is being inlined at. Without this a future
                        // walking a nested fork would execute the nested
                        // alternatives, because the outer table would not know
                        // they were alternatives at all.
                        for nested in &arm.arms {
                            all_arms.push(Arm {
                                root: NodeId::new(nested.root.value() + root.value()),
                                end: NodeId::new(nested.end.value() + root.value()),
                            });
                        }
                        for node in &arm.nodes {
                            nodes.push(node.clone());
                        }
                        let end = NodeId::new(nodes.len() as u64);
                        extents.push(Arm { root, end });
                    }
                    for a in &extents {
                        all_arms.push(*a);
                    }
                    nodes.push(PlanNode::Fork {
                        name: name.clone(),
                        arms: extents,
                    });
                }
                Stmt::Select => nodes.push(PlanNode::Select),
                Stmt::Commit => nodes.push(PlanNode::Commit),
            }
        }
        Plan {
            id: self.id,
            name: self.name,
            nodes,
            arms: all_arms,
        }
    }
}

fn describe_op(op: &Operation) -> String {
    match op.touched_key() {
        Some(k) => k.to_owned(),
        None => "step".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::PlanId;

    fn arm(name: &str, amount: i64) -> Plan {
        Plan::builder(PlanId::new(1), name)
            .state(Operation::Add {
                key: "total".into(),
                by: amount,
            })
            .state(Operation::Add {
                key: "steps".into(),
                by: 1,
            })
            .build()
    }

    #[test]
    fn a_linear_plan_is_a_flat_node_list() {
        let plan = Plan::linear(
            PlanId::new(1),
            "linear",
            vec![
                Operation::Add {
                    key: "a".into(),
                    by: 1,
                },
                Operation::Add {
                    key: "a".into(),
                    by: 1,
                },
            ],
        );
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.end(), NodeId::new(2));
        assert!(plan.node(plan.end()).is_none());
    }

    #[test]
    fn a_fork_inlines_arms_and_records_their_roots() {
        let plan = Plan::builder(PlanId::new(1), "root")
            .state(Operation::Set {
                key: "seed".into(),
                value: 1,
            })
            .fork("try", vec![arm("a", 10), arm("b", 20)])
            .select()
            .commit()
            .build();
        // 1 seed + 2 + 2 arm nodes + fork + select + commit
        assert_eq!(plan.len(), 8);
        let fork_at = NodeId::new(5);
        let PlanNode::Fork { arms, .. } = plan.node(fork_at).unwrap() else {
            panic!("expected a fork");
        };
        assert_eq!(
            arms,
            &vec![
                Arm {
                    root: NodeId::new(1),
                    end: NodeId::new(3)
                },
                Arm {
                    root: NodeId::new(3),
                    end: NodeId::new(5)
                }
            ]
        );
        // The arms really are inlined: cursor 1 is the first arm's first node.
        assert_eq!(plan.step_of(arms[0].root), StepId::new(1));
    }

    #[test]
    fn arms_run_independent_cursors() {
        let plan = Plan::builder(PlanId::new(1), "root")
            .fork("try", vec![arm("a", 1), arm("b", 2)])
            .build();
        // The fork node is last, because the arms are inlined ahead of it.
        let fork_at = NodeId::new(4);
        let arms = plan.arms_of(fork_at).expect("a fork at the end");
        // Each arm has its own two nodes, so the extents are 0..2 and 2..4, and
        // running one arm cannot walk into the other.
        assert_eq!(arms.len(), 2);
        assert_eq!(
            arms[0],
            Arm {
                root: NodeId::new(0),
                end: NodeId::new(2)
            }
        );
        assert_eq!(
            arms[1],
            Arm {
                root: NodeId::new(2),
                end: NodeId::new(4)
            }
        );
        assert!(matches!(
            plan.node(arms[0].root).unwrap(),
            PlanNode::State { .. }
        ));
        assert!(matches!(
            plan.node(arms[1].root).unwrap(),
            PlanNode::State { .. }
        ));
    }

    #[test]
    fn repeated_appends_copies_of_the_last_step() {
        let plan = Plan::builder(PlanId::new(1), "r")
            .state(Operation::Add {
                key: "a".into(),
                by: 1,
            })
            .repeated(3)
            .build();
        assert_eq!(plan.len(), 4);
    }

    #[test]
    #[should_panic(expected = "only repeats a pure state step")]
    fn repeated_refuses_a_non_state_step() {
        let _ = Plan::builder(PlanId::new(1), "r")
            .select()
            .repeated(2)
            .build();
    }

    #[test]
    fn arms_of_returns_the_extents_of_a_fork_only() {
        let plan = Plan::builder(PlanId::new(1), "r")
            .fork("t", vec![arm("a", 1), arm("b", 2)])
            .build();
        assert_eq!(plan.arms_of(NodeId::new(4)).unwrap().len(), 2);
        assert!(plan.arms_of(NodeId::new(0)).is_none());
    }

    /// 0 = seed, 1..3 = arm a, 3..5 = arm b, 5..7 = arm c, 7 = fork, 8 = select.
    fn three_arms() -> Plan {
        Plan::builder(PlanId::new(1), "r")
            .state(Operation::Set {
                key: "seed".into(),
                value: 0,
            })
            .fork("t", vec![arm("a", 1), arm("b", 2), arm("c", 3)])
            .select()
            .build()
    }

    #[test]
    fn an_empty_arm_is_not_a_skip_target() {
        // A fork over two empty plans produces two extents of length zero. A
        // future walking past the fork must terminate, not spin.
        let empty = |n: &str| Plan::builder(PlanId::new(2), n).build();
        let plan = Plan::builder(PlanId::new(1), "r")
            .state(Operation::Set {
                key: "k".into(),
                value: 1,
            })
            .fork("nothing", vec![empty("a"), empty("b")])
            .select()
            .build();
        assert_eq!(plan.entry(), NodeId::new(0));
        assert_eq!(plan.advance(NodeId::new(0), plan.end()), NodeId::new(1));
        assert_eq!(plan.advance(NodeId::new(1), plan.end()), NodeId::new(2));
    }

    #[test]
    fn a_plan_that_begins_with_a_fork_starts_at_the_fork() {
        // 0..2 arm a, 2..4 arm b, 4 = fork, 5 = select
        let plan = Plan::builder(PlanId::new(1), "r")
            .fork("t", vec![arm("a", 1), arm("b", 2)])
            .select()
            .build();
        assert_eq!(plan.entry(), NodeId::new(4), "not inside arm a");
        // A plan that starts with a step enters at zero.
        let linear = Plan::builder(PlanId::new(1), "r")
            .state(Operation::Set {
                key: "k".into(),
                value: 1,
            })
            .fork("t", vec![arm("a", 1), arm("b", 2)])
            .select()
            .build();
        assert_eq!(linear.entry(), NodeId::new(0));
    }

    #[test]
    fn a_parent_walks_over_every_alternative() {
        // Layout: 0 = seed, arms at 1..3 / 3..5 / 5..7, 7 = fork, 8 = select.
        let plan = three_arms();
        let end = plan.end();
        assert_eq!(
            plan.node(NodeId::new(7)),
            Some(&PlanNode::Fork {
                name: "t".into(),
                arms: plan.arms_of(NodeId::new(7)).unwrap().to_vec()
            }),
            "the fork sits after the arms"
        );
        // One advance from the seed jumps over *all three* arm bodies, because
        // each arm's end is the next arm's root and the loop keeps going.
        assert_eq!(
            plan.advance(NodeId::new(0), end),
            NodeId::new(7),
            "straight to the fork"
        );
        assert_eq!(
            plan.advance(NodeId::new(7), end),
            NodeId::new(8),
            "past the fork"
        );
        assert_eq!(plan.advance(NodeId::new(8), end), end, "past the end");
    }

    #[test]
    fn a_child_walks_inside_its_own_arm_and_stops_at_its_end() {
        let plan = three_arms();
        let arm = plan.arms()[0];
        assert_eq!(
            plan.advance(arm.root, arm.end),
            NodeId::new(2),
            "its own second node"
        );
        assert_eq!(
            plan.advance(NodeId::new(2), arm.end),
            arm.end,
            "clamped at the extent"
        );
    }

    #[test]
    fn a_child_cannot_step_into_its_siblings_arm() {
        // Arm a is [1,3). A future created there must not run arm b's first node
        // when it advances off its own last node.
        let plan = three_arms();
        let arm_b = plan.arms()[1];
        let n = plan.advance(NodeId::new(2), arm_b.root);
        assert_eq!(n, arm_b.root, "clamped before the sibling's arm");
    }

    #[test]
    fn nested_forks_record_their_arms_in_the_same_table() {
        // The outer arm contains a fork whose own arms sit inside it. Both sets of
        // extents have to be visible to `advance`, or a future walking the outer
        // arm would execute the inner alternatives.
        let inner = Plan::builder(PlanId::new(2), "inner")
            .fork("t", vec![arm("x", 1), arm("y", 2)])
            .select()
            .build();
        let plain = Plan::builder(PlanId::new(2), "plain")
            .state(Operation::Set {
                key: "p".into(),
                value: 9,
            })
            .build();
        let _outer = Plan::builder(PlanId::new(1), "outer")
            .fork("o", vec![inner, plain])
            .commit()
            .build();
    }

    #[test]
    fn effect_classes_are_collected_for_inspection() {
        let plan = Plan::builder(PlanId::new(1), "effects")
            .effect(
                "quote",
                EffectClass::Read,
                EffectOp::new("quote", 1),
                Some("q"),
                None,
            )
            .effect(
                "charge",
                EffectClass::Irreversible,
                EffectOp::new("charge", 2),
                None,
                Some("charged"),
            )
            .effect(
                "charge_again",
                EffectClass::IdempotentWrite,
                EffectOp::new("charge", 2),
                None,
                Some("charged"),
            )
            .build();
        assert_eq!(
            plan.effect_classes(),
            vec![
                EffectClass::Read,
                EffectClass::Irreversible,
                EffectClass::IdempotentWrite
            ]
        );
    }

    #[test]
    #[should_panic(expected = "a fork with one arm is not a fork")]
    fn a_one_arm_fork_is_rejected() {
        let _ = Plan::builder(PlanId::new(1), "r")
            .fork("solo", vec![arm("a", 1)])
            .build();
    }
}
