use crate::{Expression, Operator, Selection, Selector, Statement, Value};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticErrorKind {
    Unresolved,
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticError {
    pub kind: SemanticErrorKind,
    pub message: String,
}

impl SemanticError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: SemanticErrorKind::Invalid,
            message: message.into(),
        }
    }

    pub fn unresolved(message: impl Into<String>) -> Self {
        Self {
            kind: SemanticErrorKind::Unresolved,
            message: message.into(),
        }
    }
}

impl fmt::Display for SemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SemanticError {}

type Result<T> = std::result::Result<T, SemanticError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSelection {
    pub units: Vec<String>,
    pub ordered: bool,
    pub coordinates: Option<Vec<String>>,
}

pub trait Resolver {
    fn resolve(&self, selection: &Selection) -> Result<ResolvedSelection>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub reference: String,
    pub extent: u64,
    pub coordinate: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedExpression {
    pub members: Vec<Member>,
    pub ordered: bool,
    pub dynamic: bool,
    pub total_extent: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alignment {
    pub left: usize,
    pub right: usize,
    pub left_offset: u64,
    pub right_offset: u64,
    pub extent: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    pub operator: Operator,
    pub left: ResolvedExpression,
    pub right: ResolvedExpression,
    pub alignments: Vec<Alignment>,
    pub left_unmapped_extent: u64,
    pub right_unmapped_extent: u64,
}

pub fn evaluate(statement: &Statement, resolver: &(impl Resolver + ?Sized)) -> Result<Evaluation> {
    let left = resolve_expression(&statement.left, resolver)?;
    let right = resolve_expression(&statement.right, resolver)?;
    let positional =
        left.ordered && right.ordered || left.members.len() == 1 && right.members.len() == 1;
    match statement.operator {
        Operator::Exact => {
            require(
                left.members.len() == right.members.len(),
                "exact mapping has unequal cardinality",
            )?;
            require(
                positional,
                "unordered selections cannot be implicitly zipped",
            )?;
            require(
                left.members
                    .iter()
                    .zip(&right.members)
                    .all(|(left, right)| left.extent == right.extent),
                "exact mapping has unequal corresponding extents",
            )?;
            let coordinates = |expression: &ResolvedExpression| {
                expression
                    .members
                    .iter()
                    .map(|member| member.coordinate.clone())
                    .collect::<Vec<_>>()
            };
            require(
                coordinates(&left) == coordinates(&right),
                "recursive exact mapping has non-corresponding descendant coordinates",
            )?;
        }
        Operator::Coverage | Operator::Implication => {
            if statement.operator == Operator::Implication {
                require(
                    positional,
                    "unordered selections cannot be aligned by extent",
                )?;
            }
            if !left.dynamic && !right.dynamic {
                require(
                    left.total_extent == right.total_extent,
                    "closed mapping has unequal total extent",
                )?;
            }
            if !positional {
                require(
                    left.total_extent == right.total_extent,
                    "unordered dynamic coverage has ambiguous unmatched extent",
                )?;
            }
        }
    }
    let shared = left.total_extent.min(right.total_extent);
    let alignments = if positional {
        align(&left, &right)
    } else {
        Vec::new()
    };
    Ok(Evaluation {
        operator: statement.operator,
        left_unmapped_extent: left.total_extent - shared,
        right_unmapped_extent: right.total_extent - shared,
        left,
        right,
        alignments,
    })
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(SemanticError::invalid(message))
    }
}

fn resolve_expression(
    expression: &Expression,
    resolver: &(impl Resolver + ?Sized),
) -> Result<ResolvedExpression> {
    let mut result = match expression {
        Expression::Selection(selection) => {
            let source = selection
                .reference
                .split_once("://")
                .map(|(source, _)| source)
                .ok_or_else(|| SemanticError::invalid("invalid source reference"))?;
            let resolved = resolver.resolve(selection)?;
            require(
                !resolved.units.is_empty(),
                "selection resolves to no synchronization units",
            )?;
            let recursive = matches!(selection.selector, Some(Selector::Recursive));
            require(
                recursive == resolved.coordinates.is_some(),
                "coordinates must be supplied exactly for recursive selections",
            )?;
            if let Some(coordinates) = &resolved.coordinates {
                require(
                    coordinates.len() == resolved.units.len()
                        && coordinates.iter().all(|coordinate| !coordinate.is_empty())
                        && coordinates.iter().collect::<BTreeSet<_>>().len() == coordinates.len(),
                    "invalid or duplicate recursive coordinates",
                )?;
            }
            for unit in &resolved.units {
                require(
                    unit.split_once("://").is_some_and(|(namespace, opaque)| {
                        namespace == source
                            && !opaque.is_empty()
                            && !opaque.chars().any(|character| {
                                character.is_ascii_control()
                                    || character.is_ascii_whitespace()
                                    || ",<>[]".contains(character)
                            })
                    }),
                    "adapter returned an invalid or wrong-source unit",
                )?;
            }
            if selection.selector.is_none() {
                require(
                    resolved.units == [selection.reference.clone()],
                    "bare references must resolve only to themselves",
                )?;
            }
            if let Some(Selector::Predicates(predicates)) = &selection.selector {
                if predicates
                    .values()
                    .any(|value| matches!(value, Value::Set(values) if values.len() > 1))
                {
                    require(
                        !resolved.ordered,
                        "adapter cannot impose order on an unordered set",
                    )?;
                } else if predicates
                    .values()
                    .any(|value| matches!(value, Value::Range(..)))
                {
                    require(
                        resolved.ordered,
                        "adapter must define deterministic ordering for ranges",
                    )?;
                }
            }
            let members = resolved
                .units
                .into_iter()
                .enumerate()
                .map(|(index, reference)| Member {
                    reference,
                    extent: selection.extent.get(),
                    coordinate: resolved
                        .coordinates
                        .as_ref()
                        .map(|coordinates| coordinates[index].clone()),
                })
                .collect();
            ResolvedExpression {
                members,
                ordered: resolved.ordered,
                dynamic: is_dynamic(selection),
                total_extent: 0,
            }
        }
        Expression::Composite(expressions) => {
            require(!expressions.is_empty(), "empty composite")?;
            let children = expressions
                .iter()
                .map(|child| resolve_expression(child, resolver))
                .collect::<Result<Vec<_>>>()?;
            let recursive = children.iter().any(|child| {
                child
                    .members
                    .iter()
                    .any(|member| member.coordinate.is_some())
            });
            let mut result = ResolvedExpression {
                members: Vec::new(),
                ordered: true,
                dynamic: false,
                total_extent: 0,
            };
            for (index, mut child) in children.into_iter().enumerate() {
                result.ordered &= child.ordered || child.members.len() == 1;
                result.dynamic |= child.dynamic;
                if recursive {
                    for (ordinal, member) in child.members.iter_mut().enumerate() {
                        member.coordinate = Some(format!(
                            "{index}/{}",
                            member
                                .coordinate
                                .clone()
                                .unwrap_or_else(|| format!("@{ordinal}"))
                        ));
                    }
                }
                result.members.extend(child.members);
            }
            result
        }
    };
    require(
        result
            .members
            .iter()
            .map(|member| &member.reference)
            .collect::<BTreeSet<_>>()
            .len()
            == result.members.len(),
        "expression repeats a synchronization unit",
    )?;
    result.total_extent = result
        .members
        .iter()
        .try_fold(0_u64, |total, member| total.checked_add(member.extent))
        .ok_or_else(|| {
            SemanticError::invalid("total extent exceeds the supported integer range")
        })?;
    Ok(result)
}

fn is_dynamic(selection: &Selection) -> bool {
    match &selection.selector {
        None => false,
        Some(Selector::Recursive) => true,
        Some(Selector::Predicates(predicates)) => predicates.values().any(|value| {
            matches!(
                value,
                Value::Wildcard | Value::Range(None, _) | Value::Range(_, None)
            )
        }),
    }
}

fn align(left: &ResolvedExpression, right: &ResolvedExpression) -> Vec<Alignment> {
    let mut segments = Vec::new();
    let (mut left_index, mut right_index, mut left_offset, mut right_offset) = (0, 0, 0, 0);
    while left_index < left.members.len() && right_index < right.members.len() {
        let extent = (left.members[left_index].extent - left_offset)
            .min(right.members[right_index].extent - right_offset);
        segments.push(Alignment {
            left: left_index,
            right: right_index,
            left_offset,
            right_offset,
            extent,
        });
        left_offset += extent;
        right_offset += extent;
        if left_offset == left.members[left_index].extent {
            left_index += 1;
            left_offset = 0;
        }
        if right_offset == right.members[right_index].extent {
            right_index += 1;
            right_offset = 0;
        }
    }
    segments
}
