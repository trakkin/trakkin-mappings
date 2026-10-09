use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use trakkin_mappings_language::{
    ClaimId, Record, ResolvedSelection, Resolver, Selection, SemanticError, parse_record,
};

const DECISION: &str = "#@decision ";
const RESOLUTION: &str = "#@resolve ";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub units: Vec<String>,
    pub ordered: bool,
    pub coordinates: Option<Vec<String>>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Bindings(pub BTreeMap<String, Resolution>);

impl Bindings {
    pub fn read(record: &Record) -> Result<Self> {
        let mut found = None;
        for line in &record.metadata {
            if let Some(value) = line.strip_prefix(RESOLUTION) {
                ensure!(found.is_none(), "duplicate selector bindings");
                found = Some(serde_json::from_str::<Self>(value)?);
            }
        }
        let bindings = found.unwrap_or_default();
        for (key, resolution) in &bindings.0 {
            ensure!(
                resolution.units.len() <= 100_000,
                "selector has too many units"
            );
            ensure!(
                resolution
                    .coordinates
                    .as_ref()
                    .is_none_or(|coordinates| coordinates.len() == resolution.units.len()),
                "inconsistent selector coordinates"
            );
            for unit in &resolution.units {
                trakkin_mappings_language::parse_reference(unit)?;
            }
            ensure!(
                record
                    .statement
                    .left
                    .selections()
                    .into_iter()
                    .chain(record.statement.right.selections())
                    .any(|selection| selection.selector.is_some()
                        && selection.selection_key() == *key),
                "binding does not belong to this claim"
            );
        }
        Ok(bindings)
    }

    pub fn write(&self, record: &mut Record) -> Result<()> {
        record.metadata.retain(|line| !line.starts_with(RESOLUTION));
        if !self.0.is_empty() {
            record
                .metadata
                .push(format!("{RESOLUTION}{}", serde_json::to_string(self)?));
        }
        Ok(())
    }
}

impl Resolver for Bindings {
    fn resolve(&self, selection: &Selection) -> Result<ResolvedSelection, SemanticError> {
        if selection.selector.is_none() {
            return Ok(ResolvedSelection {
                units: vec![selection.reference.clone()],
                ordered: true,
                coordinates: None,
            });
        }
        let resolution = self.0.get(&selection.selection_key()).ok_or_else(|| {
            SemanticError::unresolved("selector has no published native resolution")
        })?;
        if let Some(error) = &resolution.error {
            return Err(SemanticError::unresolved(error.clone()));
        }
        if resolution.units.iter().any(|unit| {
            unit.split_once("://").map(|(source, _)| source) != Some(selection.source())
        }) {
            return Err(SemanticError::invalid(
                "selector binding crosses source namespaces",
            ));
        }
        Ok(ResolvedSelection {
            units: resolution.units.clone(),
            ordered: resolution.ordered,
            coordinates: resolution.coordinates.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Artifact {
    pub record: Record,
    pub rejected: bool,
}

pub fn claim_path(id: &ClaimId) -> String {
    format!("mappings/v1/claims/{}/{}.trk", &id.hash()[..2], id.hash())
}

impl Artifact {
    pub fn path(&self) -> String {
        claim_path(&self.record.statement.claim_id())
    }

    pub fn text(&self) -> String {
        let mut record = self.record.clone();
        record.metadata.retain(|line| !line.starts_with(DECISION));
        record.metadata.push(format!(
            "{DECISION}{}",
            if self.rejected {
                "rejected"
            } else {
                "accepted"
            }
        ));
        record.canonical()
    }

    pub fn parse(path: &str, text: &str) -> Result<Self> {
        ensure!(text.len() <= 2 * 1024 * 1024, "mapping file exceeds 2 MiB");
        let mut record = parse_record(text)?;
        ensure!(record.canonical() == text, "noncanonical mapping file");
        let decisions = record
            .metadata
            .iter()
            .filter_map(|line| line.strip_prefix(DECISION))
            .collect::<Vec<_>>();
        ensure!(
            decisions.len() == 1,
            "mapping file requires one decision annotation"
        );
        let rejected = match decisions[0] {
            "accepted" => false,
            "rejected" => true,
            _ => anyhow::bail!("invalid mapping decision"),
        };
        Bindings::read(&record)?;
        record.metadata.retain(|line| !line.starts_with(DECISION));
        let artifact = Self { record, rejected };
        ensure!(
            artifact.path() == path,
            "mapping file has an incorrect claim path"
        );
        ensure!(
            artifact.text() == text,
            "mapping decision annotation must be last"
        );
        Ok(artifact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trakkin_mappings_language::evaluate;

    #[test]
    fn publication_is_one_canonical_file_per_claim() {
        let artifact = Artifact {
            record: parse_record("a://1 <=> b://2\n").unwrap(),
            rejected: true,
        };
        let decoded = Artifact::parse(&artifact.path(), &artifact.text()).unwrap();
        assert!(decoded.rejected);
        assert_eq!(decoded.record, artifact.record);
        assert!(Artifact::parse("mappings/v1/wrong.trk", &artifact.text()).is_err());
    }

    #[test]
    fn shared_bindings_preserve_selector_order_and_extent_evaluation() {
        let mut record =
            parse_record("a://show :: episode=1..2 @2 <~> [b://1,b://2,b://3,b://4]\n").unwrap();
        let selection = record
            .statement
            .left
            .selections()
            .into_iter()
            .chain(record.statement.right.selections())
            .find(|selection| selection.selector.is_some())
            .unwrap();
        let bindings = Bindings(BTreeMap::from([(
            selection.selection_key(),
            Resolution {
                units: vec!["a://episode/1".into(), "a://episode/2".into()],
                ordered: true,
                coordinates: None,
                error: None,
            },
        )]));
        bindings.write(&mut record).unwrap();
        let published = Artifact {
            record,
            rejected: false,
        };
        let imported = Artifact::parse(&published.path(), &published.text()).unwrap();
        let resolution = evaluate(
            &imported.record.statement,
            &Bindings::read(&imported.record).unwrap(),
        )
        .unwrap();
        assert_eq!(resolution.alignments.len(), 4);
    }
}
