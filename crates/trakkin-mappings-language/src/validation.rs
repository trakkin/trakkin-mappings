use crate::{Expression, Operator, Selection, Selector, Statement, Value};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolved {
    pub items: Vec<String>,
    pub ordered: bool,
    #[serde(default)]
    pub coordinates: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedExpression {
    pub items: Vec<String>,
    pub extents: Vec<u64>,
    pub ordered: bool,
    #[serde(default)]
    pub coordinates: Option<Vec<String>>,
}

pub trait Resolver {
    fn resolve(&self, selection: &Selection) -> Result<Resolved>;
}

pub fn validate(
    statement: &Statement,
    resolver: &impl Resolver,
) -> Result<(ResolvedExpression, ResolvedExpression)> {
    let left = resolve_expression(&statement.left, resolver)?;
    let right = resolve_expression(&statement.right, resolver)?;
    match statement.operator {
        Operator::Exact => {
            ensure!(
                left.items.len() == right.items.len(),
                "exact mapping has unequal cardinality"
            );
            ensure!(
                left.items.len() == 1 || (left.ordered && right.ordered),
                "unordered selections cannot be implicitly zipped"
            );
            ensure!(
                left.extents == right.extents,
                "exact mapping has unequal corresponding extents"
            );
            if left.coordinates.is_some() || right.coordinates.is_some() {
                ensure!(
                    left.coordinates.is_some() && left.coordinates == right.coordinates,
                    "recursive exact mapping has non-corresponding descendant coordinates"
                );
            }
        }
        Operator::Coverage => {
            validate_total_extent(statement, &left, &right)?;
        }
        Operator::Implication => {
            ensure!(
                (left.items.len() == 1 && right.items.len() == 1)
                    || (left.ordered && right.ordered),
                "unordered selections cannot be aligned by extent"
            );
            validate_total_extent(statement, &left, &right)?;
        }
    }
    Ok((left, right))
}

fn validate_total_extent(
    statement: &Statement,
    left: &ResolvedExpression,
    right: &ResolvedExpression,
) -> Result<()> {
    let left_total = total_extent(left)?;
    let right_total = total_extent(right)?;
    if expression_is_closed(&statement.left) && expression_is_closed(&statement.right) {
        ensure!(
            left_total == right_total,
            "closed mapping has unequal total extent ({left_total} != {right_total})"
        );
    }
    Ok(())
}

fn total_extent(resolved: &ResolvedExpression) -> Result<u64> {
    resolved.extents.iter().try_fold(0_u64, |total, extent| {
        total
            .checked_add(*extent)
            .context("resolved expression total extent exceeds the supported integer range")
    })
}

fn expression_is_closed(expression: &Expression) -> bool {
    match expression {
        Expression::Selection(selection) => match &selection.selector {
            None => true,
            Some(Selector::Recursive) => false,
            Some(Selector::Predicates(predicates)) => predicates.values().all(|value| {
                matches!(
                    value,
                    Value::Scalar(_) | Value::Set(_) | Value::Range(Some(_), Some(_))
                )
            }),
        },
        Expression::Composite(expressions) => expressions.iter().all(expression_is_closed),
    }
}

fn resolve_expression(
    expression: &Expression,
    resolver: &impl Resolver,
) -> Result<ResolvedExpression> {
    match expression {
        Expression::Selection(selection) => {
            let result = resolver.resolve(selection)?;
            ensure!(
                !result.items.is_empty(),
                "selection resolves to no synchronization units"
            );
            ensure!(
                result.items.iter().all(|item| !item.is_empty())
                    && result.items.iter().collect::<BTreeSet<_>>().len() == result.items.len(),
                "selection has invalid or repeated units"
            );
            if matches!(selection.selector, Some(Selector::Recursive)) {
                ensure!(
                    result.coordinates.is_some(),
                    "recursive selection requires stable coordinates"
                );
            } else {
                ensure!(
                    result.coordinates.is_none(),
                    "coordinates are only valid for recursive selections"
                );
            }
            if let Some(coordinates) = &result.coordinates {
                ensure!(
                    coordinates.len() == result.items.len()
                        && coordinates.iter().collect::<BTreeSet<_>>().len() == coordinates.len(),
                    "invalid recursive coordinates"
                );
            }
            if let Some(Selector::Predicates(predicates)) = &selection.selector {
                let unordered = predicates
                    .values()
                    .any(|value| matches!(value, crate::Value::Set(values) if values.len() > 1));
                if unordered {
                    ensure!(
                        !result.ordered,
                        "adapter cannot impose order on an unordered set"
                    );
                } else if predicates
                    .values()
                    .any(|value| matches!(value, crate::Value::Range(..)))
                {
                    ensure!(
                        result.ordered,
                        "adapter must define deterministic ordering for ranges"
                    );
                }
            }
            Ok(ResolvedExpression {
                extents: vec![selection.extent.get(); result.items.len()],
                items: result.items,
                ordered: result.ordered,
                coordinates: result.coordinates,
            })
        }
        Expression::Composite(expressions) => {
            let mut result = ResolvedExpression {
                items: Vec::new(),
                extents: Vec::new(),
                ordered: true,
                coordinates: None,
            };
            let mut coordinates = Vec::new();
            let mut recursive = false;
            for (index, expression) in expressions.iter().enumerate() {
                let resolved = resolve_expression(expression, resolver)?;
                result.ordered &= resolved.ordered || resolved.items.len() == 1;
                if let Some(child_coordinates) = resolved.coordinates {
                    recursive = true;
                    coordinates.extend(
                        child_coordinates
                            .into_iter()
                            .map(|coordinate| format!("{index}/{coordinate}")),
                    );
                } else {
                    coordinates
                        .extend((0..resolved.items.len()).map(|unit| format!("{index}/@{unit}")));
                }
                result.items.extend(resolved.items);
                result.extents.extend(resolved.extents);
            }
            ensure!(
                result.items.iter().collect::<BTreeSet<_>>().len() == result.items.len(),
                "composite repeats a synchronization unit"
            );
            if recursive {
                result.coordinates = Some(coordinates);
            }
            Ok(result)
        }
    }
}
