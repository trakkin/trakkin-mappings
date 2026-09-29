use crate::{Expression, Operator, Selection, Selector, Statement, Value, digest, parse_unit_key};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt::Debug};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolved {
    pub units: Vec<String>,
    pub ordered: bool,
    #[serde(default)]
    pub coordinates: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedExpression {
    pub units: Vec<String>,
    pub extents: Vec<u64>,
    pub ordered: bool,
    #[serde(default)]
    pub coordinates: Option<Vec<String>>,
}

pub trait Resolver: Debug + Send + Sync {
    fn evidence_fingerprint(&self) -> String;
    fn validate_selection(&self, _selection: &Selection) -> Result<()> {
        Ok(())
    }
    fn resolve(&self, selection: &Selection) -> Result<Resolved>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityResolver;

impl Resolver for IdentityResolver {
    fn evidence_fingerprint(&self) -> String {
        digest(&[b"trakkin:identity-resolution:v1\0"])
    }

    fn resolve(&self, selection: &Selection) -> Result<Resolved> {
        ensure!(
            selection.selector.is_none(),
            "selector resolution evidence is missing for {}",
            selection.selection_key()
        );
        Ok(Resolved {
            units: vec![selection.reference.clone()],
            ordered: true,
            coordinates: None,
        })
    }
}

pub fn validate(
    statement: &Statement,
    resolver: &(impl Resolver + ?Sized),
) -> Result<(ResolvedExpression, ResolvedExpression)> {
    let left = resolve_expression(&statement.left, resolver)?;
    let right = resolve_expression(&statement.right, resolver)?;
    match statement.operator {
        Operator::Exact => {
            ensure!(
                left.units.len() == right.units.len(),
                "exact mapping has unequal cardinality"
            );
            ensure!(
                left.units.len() == 1 || (left.ordered && right.ordered),
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
                (left.units.len() == 1 && right.units.len() == 1)
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
    resolver: &(impl Resolver + ?Sized),
) -> Result<ResolvedExpression> {
    match expression {
        Expression::Selection(selection) => {
            resolver.validate_selection(selection)?;
            let result = if selection.selector.is_none() {
                Resolved {
                    units: vec![selection.reference.clone()],
                    ordered: true,
                    coordinates: None,
                }
            } else {
                resolver.resolve(selection)?
            };
            ensure!(
                !result.units.is_empty(),
                "selection resolves to no synchronization units"
            );
            ensure!(
                result.units.iter().all(|unit| !unit.is_empty())
                    && result.units.iter().collect::<BTreeSet<_>>().len() == result.units.len(),
                "selection has invalid or repeated units"
            );
            for unit_key in &result.units {
                let unit =
                    parse_unit_key(unit_key).context("resolver returned an invalid unit key")?;
                ensure!(
                    unit.source() == selection.source(),
                    "resolver returned a unit outside the selection source"
                );
            }
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
                    coordinates.len() == result.units.len()
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
                extents: vec![selection.extent.get(); result.units.len()],
                units: result.units,
                ordered: result.ordered,
                coordinates: result.coordinates,
            })
        }
        Expression::Composite(expressions) => {
            let mut result = ResolvedExpression {
                units: Vec::new(),
                extents: Vec::new(),
                ordered: true,
                coordinates: None,
            };
            let mut coordinates = Vec::new();
            let mut recursive = false;
            for (index, expression) in expressions.iter().enumerate() {
                let resolved = resolve_expression(expression, resolver)?;
                result.ordered &= resolved.ordered || resolved.units.len() == 1;
                if let Some(child_coordinates) = resolved.coordinates {
                    recursive = true;
                    coordinates.extend(
                        child_coordinates
                            .into_iter()
                            .map(|coordinate| format!("{index}/{coordinate}")),
                    );
                } else {
                    coordinates
                        .extend((0..resolved.units.len()).map(|unit| format!("{index}/@{unit}")));
                }
                result.units.extend(resolved.units);
                result.extents.extend(resolved.extents);
            }
            ensure!(
                result.units.iter().collect::<BTreeSet<_>>().len() == result.units.len(),
                "composite repeats a synchronization unit"
            );
            if recursive {
                result.coordinates = Some(coordinates);
            }
            Ok(result)
        }
    }
}
