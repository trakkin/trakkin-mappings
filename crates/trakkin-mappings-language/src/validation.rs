use crate::{Expression, Operator, Selection, Selector, Statement};
use anyhow::{Result, ensure};
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

pub trait Resolver {
    fn resolve(&self, selection: &Selection) -> Result<Resolved>;
}

pub fn validate(statement: &Statement, resolver: &impl Resolver) -> Result<(Resolved, Resolved)> {
    let left = resolve_expression(&statement.left, resolver)?;
    let right = resolve_expression(&statement.right, resolver)?;
    if statement.operator != Operator::Coverage {
        ensure!(
            left.items.len() == right.items.len(),
            "positional mapping has unequal cardinality; use <~> for collective coverage"
        );
        ensure!(
            left.items.len() == 1 || (left.ordered && right.ordered),
            "unordered selections cannot be implicitly zipped"
        );
    }
    if statement.operator == Operator::Exact
        && (left.coordinates.is_some() || right.coordinates.is_some())
    {
        ensure!(
            left.coordinates.is_some() && left.coordinates == right.coordinates,
            "recursive exact mapping has non-corresponding descendant coordinates"
        );
    }
    Ok((left, right))
}

fn resolve_expression(expression: &Expression, resolver: &impl Resolver) -> Result<Resolved> {
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
            Ok(result)
        }
        Expression::Composite(expressions) => {
            let mut result = Resolved {
                items: Vec::new(),
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
