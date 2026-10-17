use std::{cell::Cell, collections::BTreeMap, fmt};

use num_rational::Ratio;
use serde::{Deserialize, Serialize};

use crate::{
    Expression, Operator, ResolvedExpression, ResolvedSelection, Resolver, Selection,
    SemanticError, Statement, evaluate,
};

pub type RationalPosition = Ratio<u64>;

const MAXIMUM_SAFE_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CorrespondenceLimits {
    pub maximum_members: usize,
    pub maximum_work: usize,
    pub maximum_depth: usize,
}

impl Default for CorrespondenceLimits {
    fn default() -> Self {
        Self {
            maximum_members: 100_000,
            maximum_work: 1_000_000,
            maximum_depth: 128,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrespondenceCapture {
    pub mapping_revision: String,
    pub reference_revisions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnitPair {
    pub left: String,
    pub right: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnitSpan {
    pub reference: String,
    pub start: RationalPosition,
    pub end: RationalPosition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MaterialAlignment {
    pub left: UnitSpan,
    pub right: UnitSpan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CorrespondenceRelation {
    Exact(Vec<UnitPair>),
    OrderedCoverage(Vec<MaterialAlignment>),
    UnorderedCoverage {
        left: Vec<String>,
        right: Vec<String>,
    },
    PositiveImplication(Vec<MaterialAlignment>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CorrespondenceTail {
    pub open: bool,
    pub unmapped: Vec<UnitSpan>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Correspondence {
    capture: CorrespondenceCapture,
    relation: CorrespondenceRelation,
    left_tail: CorrespondenceTail,
    right_tail: CorrespondenceTail,
}

impl Correspondence {
    pub fn capture(&self) -> &CorrespondenceCapture {
        &self.capture
    }
    pub fn relation(&self) -> &CorrespondenceRelation {
        &self.relation
    }
    pub fn left_tail(&self) -> &CorrespondenceTail {
        &self.left_tail
    }
    pub fn right_tail(&self) -> &CorrespondenceTail {
        &self.right_tail
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CorrespondenceError {
    InvalidCapture,
    LimitExceeded,
    Semantics(SemanticError),
}

impl fmt::Display for CorrespondenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapture => {
                formatter.write_str("correspondence revisions are missing or invalid")
            }
            Self::LimitExceeded => {
                formatter.write_str("correspondence analysis exceeded its limits")
            }
            Self::Semantics(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CorrespondenceError {}

struct BoundedResolver<'a, R: ?Sized> {
    inner: &'a R,
    maximum_members: usize,
    remaining_members: Cell<usize>,
    remaining_work: Cell<usize>,
    maximum_depth: usize,
    exceeded: Cell<bool>,
}

impl<R: ?Sized> BoundedResolver<'_, R> {
    fn spend(&self, amount: usize) -> Result<(), SemanticError> {
        let Some(remaining) = self.remaining_work.get().checked_sub(amount) else {
            self.exceeded.set(true);
            return Err(SemanticError::invalid("correspondence work limit exceeded"));
        };
        self.remaining_work.set(remaining);
        Ok(())
    }
}

impl<R: Resolver + ?Sized> Resolver for BoundedResolver<'_, R> {
    fn resolve(&self, selection: &Selection) -> Result<ResolvedSelection, SemanticError> {
        let resolved = self.inner.resolve(selection)?;
        let count = resolved.units.len();
        let Some(remaining) = self.remaining_members.get().checked_sub(count) else {
            self.exceeded.set(true);
            return Err(SemanticError::invalid(
                "correspondence member limit exceeded",
            ));
        };
        if count > self.maximum_members
            || resolved.units.iter().any(|unit| unit.len() > 4096)
            || resolved.coordinates.as_ref().is_some_and(|coordinates| {
                coordinates.len() > self.maximum_members
                    || coordinates.iter().any(|coordinate| coordinate.len() > 4096)
            })
        {
            self.exceeded.set(true);
            return Err(SemanticError::invalid(
                "correspondence member limit exceeded",
            ));
        }
        let bytes = resolved
            .units
            .iter()
            .map(|unit| unit.len())
            .chain(
                resolved
                    .coordinates
                    .iter()
                    .flatten()
                    .map(|coordinate| coordinate.len()),
            )
            .try_fold(count, |total, length| total.checked_add(length));
        let Some(work) = bytes.and_then(|bytes| bytes.checked_mul(self.maximum_depth)) else {
            self.exceeded.set(true);
            return Err(SemanticError::invalid("correspondence work limit exceeded"));
        };
        self.spend(work)?;
        self.remaining_members.set(remaining);
        Ok(resolved)
    }
}

pub fn compile_correspondence(
    statement: &Statement,
    resolver: &(impl Resolver + ?Sized),
    capture: CorrespondenceCapture,
    limits: CorrespondenceLimits,
) -> Result<Correspondence, CorrespondenceError> {
    let valid_revision = |revision: &str| {
        !revision.is_empty() && revision.len() <= 4096 && !revision.chars().any(char::is_control)
    };
    if limits.maximum_members == 0
        || limits.maximum_work == 0
        || limits.maximum_depth == 0
        || capture.reference_revisions.len() > limits.maximum_members
    {
        return Err(CorrespondenceError::LimitExceeded);
    }
    if !valid_revision(&capture.mapping_revision) || capture.reference_revisions.is_empty() {
        return Err(CorrespondenceError::InvalidCapture);
    }
    let mut remaining_work = limits
        .maximum_work
        .checked_sub(capture.mapping_revision.len())
        .ok_or(CorrespondenceError::LimitExceeded)?;
    for (source, revision) in &capture.reference_revisions {
        remaining_work = remaining_work
            .checked_sub(source.len())
            .and_then(|remaining| remaining.checked_sub(revision.len()))
            .ok_or(CorrespondenceError::LimitExceeded)?;
        if source.is_empty() || source.len() > 4096 || !valid_revision(revision) {
            return Err(CorrespondenceError::InvalidCapture);
        }
    }
    let mut maximum_depth = 1;
    let mut stack = vec![(&statement.left, 1), (&statement.right, 1)];
    while let Some((expression, depth)) = stack.pop() {
        remaining_work = remaining_work
            .checked_sub(1)
            .ok_or(CorrespondenceError::LimitExceeded)?;
        if depth > limits.maximum_depth.min(MAXIMUM_SAFE_DEPTH) {
            return Err(CorrespondenceError::LimitExceeded);
        }
        maximum_depth = maximum_depth.max(depth);
        match expression {
            Expression::Selection(selection) => {
                if selection.reference.len() > 4096 {
                    return Err(CorrespondenceError::LimitExceeded);
                }
                let Some((source, _)) = selection.reference.split_once("://") else {
                    return Err(CorrespondenceError::Semantics(SemanticError::invalid(
                        "invalid source reference",
                    )));
                };
                if !capture.reference_revisions.contains_key(source) {
                    return Err(CorrespondenceError::InvalidCapture);
                }
            }
            Expression::Composite(expressions) => {
                if expressions.len() > remaining_work.saturating_sub(stack.len()) {
                    return Err(CorrespondenceError::LimitExceeded);
                }
                stack.extend(expressions.iter().map(|expression| (expression, depth + 1)));
            }
        }
    }
    let bounded = BoundedResolver {
        inner: resolver,
        maximum_members: limits.maximum_members,
        remaining_members: Cell::new(limits.maximum_members),
        remaining_work: Cell::new(remaining_work),
        maximum_depth,
        exceeded: Cell::new(false),
    };
    let evaluation = evaluate(statement, &bounded).map_err(|error| {
        if bounded.exceeded.get() {
            CorrespondenceError::LimitExceeded
        } else {
            CorrespondenceError::Semantics(error)
        }
    })?;
    bounded
        .spend(evaluation.alignments.len())
        .map_err(|_| CorrespondenceError::LimitExceeded)?;
    let segments = || {
        evaluation
            .alignments
            .iter()
            .map(|alignment| {
                let left = &evaluation.left.members[alignment.left];
                let right = &evaluation.right.members[alignment.right];
                MaterialAlignment {
                    left: UnitSpan {
                        reference: left.reference.clone(),
                        start: Ratio::new(alignment.left_offset, left.extent),
                        end: Ratio::new(alignment.left_offset + alignment.extent, left.extent),
                    },
                    right: UnitSpan {
                        reference: right.reference.clone(),
                        start: Ratio::new(alignment.right_offset, right.extent),
                        end: Ratio::new(alignment.right_offset + alignment.extent, right.extent),
                    },
                }
            })
            .collect()
    };
    let relation = match evaluation.operator {
        Operator::Exact => CorrespondenceRelation::Exact(
            evaluation
                .left
                .members
                .iter()
                .zip(&evaluation.right.members)
                .map(|(left, right)| UnitPair {
                    left: left.reference.clone(),
                    right: right.reference.clone(),
                })
                .collect(),
        ),
        Operator::Coverage if evaluation.alignments.is_empty() => {
            let references = |expression: &ResolvedExpression| {
                let mut references: Vec<_> = expression
                    .members
                    .iter()
                    .map(|member| member.reference.clone())
                    .collect();
                references.sort();
                references
            };
            CorrespondenceRelation::UnorderedCoverage {
                left: references(&evaluation.left),
                right: references(&evaluation.right),
            }
        }
        Operator::Coverage => CorrespondenceRelation::OrderedCoverage(segments()),
        Operator::Implication => CorrespondenceRelation::PositiveImplication(segments()),
    };
    let shared = evaluation
        .left
        .total_extent
        .min(evaluation.right.total_extent);
    Ok(Correspondence {
        capture,
        relation,
        left_tail: tail(&evaluation.left, shared),
        right_tail: tail(&evaluation.right, shared),
    })
}

fn tail(expression: &ResolvedExpression, mut shared: u64) -> CorrespondenceTail {
    let mut unmapped = Vec::new();
    for member in &expression.members {
        let covered = shared.min(member.extent);
        shared -= covered;
        if covered < member.extent {
            unmapped.push(UnitSpan {
                reference: member.reference.clone(),
                start: Ratio::new(covered, member.extent),
                end: Ratio::from_integer(1),
            });
        }
    }
    CorrespondenceTail {
        open: expression.dynamic,
        unmapped,
    }
}
