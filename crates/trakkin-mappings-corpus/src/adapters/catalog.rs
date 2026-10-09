use std::collections::{BTreeMap, BTreeSet};
use trakkin_mappings_language::{
    ResolvedSelection, Resolver, Selection, Selector, SemanticError, Value,
};

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Catalog {
    pub entities: BTreeMap<String, Entity>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Entity {
    pub levels: Vec<String>,
    pub dimensions: BTreeMap<String, Vec<String>>,
    pub contexts: BTreeMap<String, String>,
    pub hierarchy_complete: bool,
    pub units: Vec<Unit>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Unit {
    pub reference: String,
    pub level: String,
    pub attributes: BTreeMap<String, String>,
    pub coordinate: String,
}

impl Catalog {
    pub fn finish(mut self) -> Self {
        let references = self
            .entities
            .values()
            .flat_map(|entity| &entity.units)
            .map(|unit| unit.reference.clone())
            .collect::<Vec<_>>();
        for reference in references {
            self.entities.entry(reference).or_default();
        }
        for entity in self.entities.values_mut() {
            for dimension in &entity.levels {
                let mut values = entity
                    .units
                    .iter()
                    .filter_map(|unit| unit.attributes.get(dimension).cloned())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                values.sort_by_key(|value| value.parse::<u64>().unwrap_or(u64::MAX));
                entity.dimensions.insert(dimension.clone(), values);
            }
            for context in entity.contexts.keys() {
                let values = entity
                    .units
                    .iter()
                    .map(|unit| unit.attributes.get(context).cloned().unwrap_or_default())
                    .collect::<BTreeSet<_>>();
                entity
                    .dimensions
                    .insert(context.clone(), values.into_iter().collect());
            }
        }
        self
    }
}

impl Resolver for Catalog {
    fn resolve(
        &self,
        selection: &Selection,
    ) -> std::result::Result<ResolvedSelection, SemanticError> {
        let entity = self.entities.get(&selection.reference).ok_or_else(|| {
            SemanticError::unresolved(format!(
                "{} is absent from the native catalog scope",
                selection.reference
            ))
        })?;
        let Some(selector) = &selection.selector else {
            return Ok(ResolvedSelection {
                units: vec![selection.reference.clone()],
                ordered: true,
                coordinates: None,
            });
        };
        let recursive = matches!(selector, Selector::Recursive);
        if recursive && !entity.hierarchy_complete {
            return Err(SemanticError::unresolved(format!(
                "complete hierarchy is unavailable for {}",
                selection.reference
            )));
        }
        let mut predicates = BTreeMap::new();
        let mut ordered = true;
        let level = if let Selector::Predicates(authored) = selector {
            for (dimension, value) in authored {
                let domain = entity.dimensions.get(dimension).ok_or_else(|| {
                    SemanticError::invalid(format!(
                        "{} does not support dimension {dimension}",
                        selection.reference
                    ))
                })?;
                if entity.contexts.contains_key(dimension) && !matches!(value, Value::Scalar(_)) {
                    return Err(SemanticError::invalid(format!(
                        "{dimension} must select one ordering context"
                    )));
                }
                if matches!(value, Value::Set(values) if values.len() > 1) {
                    ordered = false;
                }
                if entity.contexts.contains_key(dimension) {
                    predicates.insert(
                        dimension.clone(),
                        selected_values(value, domain, dimension)?,
                    );
                }
            }
            Some(
                entity
                    .levels
                    .iter()
                    .rev()
                    .find(|level| {
                        authored.contains_key(*level)
                            && entity.units.iter().any(|unit| &unit.level == *level)
                    })
                    .or_else(|| entity.levels.last())
                    .ok_or_else(|| {
                        SemanticError::invalid(
                            "selector must name a supported synchronization-unit dimension",
                        )
                    })?
                    .as_str(),
            )
        } else {
            None
        };
        for (context, default) in &entity.contexts {
            predicates
                .entry(context.clone())
                .or_insert_with(|| vec![default.clone()]);
        }
        let candidates = entity
            .units
            .iter()
            .filter(|unit| {
                level.is_none_or(|level| unit.level == level)
                    && predicates.iter().all(|(dimension, values)| {
                        values.iter().any(|value| {
                            value
                                == unit
                                    .attributes
                                    .get(dimension)
                                    .map(String::as_str)
                                    .unwrap_or("")
                        })
                    })
            })
            .collect::<Vec<_>>();
        if let Selector::Predicates(authored) = selector {
            for (dimension, value) in authored {
                if entity.contexts.contains_key(dimension) {
                    continue;
                }
                let present = candidates
                    .iter()
                    .filter(|unit| {
                        authored.iter().all(|(other, filter)| {
                            if other == dimension || entity.contexts.contains_key(other) {
                                return true;
                            }
                            match filter {
                                Value::Scalar(value) => unit.attributes.get(other) == Some(value),
                                Value::Set(values) => unit
                                    .attributes
                                    .get(other)
                                    .is_some_and(|value| values.contains(value)),
                                _ => true,
                            }
                        })
                    })
                    .filter_map(|unit| unit.attributes.get(dimension))
                    .collect::<BTreeSet<_>>();
                let domain = entity.dimensions[dimension]
                    .iter()
                    .filter(|value| present.contains(value))
                    .cloned()
                    .collect::<Vec<_>>();
                predicates.insert(
                    dimension.clone(),
                    selected_values(value, &domain, dimension)?,
                );
            }
        }
        let mut units = candidates
            .into_iter()
            .filter(|unit| {
                predicates.iter().all(|(dimension, values)| {
                    values.iter().any(|value| {
                        value
                            == unit
                                .attributes
                                .get(dimension)
                                .map(String::as_str)
                                .unwrap_or("")
                    })
                })
            })
            .collect::<Vec<_>>();
        units.sort_by(|left, right| {
            for dimension in &entity.levels {
                let values = predicates
                    .get(dimension)
                    .unwrap_or(&entity.dimensions[dimension]);
                let rank = |unit: &Unit| {
                    unit.attributes
                        .get(dimension)
                        .and_then(|value| values.iter().position(|candidate| candidate == value))
                };
                let order = rank(left).cmp(&rank(right));
                if !order.is_eq() {
                    return order;
                }
            }
            left.level
                .cmp(&right.level)
                .then(left.reference.cmp(&right.reference))
        });
        if units.is_empty() {
            return Err(SemanticError::unresolved(format!(
                "{} selects no units in this native catalog scope",
                selection.selection_key()
            )));
        }
        let coordinates = units
            .iter()
            .map(|unit| &unit.coordinate)
            .collect::<BTreeSet<_>>();
        if coordinates.len() != units.len() {
            return Err(SemanticError::invalid(
                "source ordering contains ambiguous coordinates",
            ));
        }
        Ok(ResolvedSelection {
            units: units.iter().map(|unit| unit.reference.clone()).collect(),
            ordered,
            coordinates: recursive
                .then(|| units.iter().map(|unit| unit.coordinate.clone()).collect()),
        })
    }
}

fn selected_values(
    value: &Value,
    domain: &[String],
    dimension: &str,
) -> std::result::Result<Vec<String>, SemanticError> {
    let position = |value: &String| {
        domain
            .iter()
            .position(|candidate| candidate == value)
            .ok_or_else(|| {
                SemanticError::unresolved(format!(
                    "{dimension}={value} is absent from the native catalog scope"
                ))
            })
    };
    match value {
        Value::Scalar(value) => {
            position(value)?;
            Ok(vec![value.clone()])
        }
        Value::Set(values) => {
            for value in values {
                position(value)?;
            }
            Ok(values.clone())
        }
        Value::Wildcard => Ok(domain.to_vec()),
        Value::Range(start, end) => {
            if domain.is_empty() {
                return Err(SemanticError::unresolved(format!(
                    "ordering for {dimension} is unavailable"
                )));
            }
            let start = start.as_ref().map(position).transpose()?.unwrap_or(0);
            let end = end
                .as_ref()
                .map(position)
                .transpose()?
                .unwrap_or(domain.len() - 1);
            let mut values = domain[start.min(end)..=start.max(end)].to_vec();
            if start > end {
                values.reverse();
            }
            Ok(values)
        }
    }
}
