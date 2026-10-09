use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const POLICY: &str = "mappings/v1/policy.json";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusiveSourcePair {
    pub origin_source: String,
    pub target_source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictPolicy {
    pub format_version: u32,
    pub exclusive_source_pairs: Vec<ExclusiveSourcePair>,
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        Self {
            format_version: 1,
            exclusive_source_pairs: Vec::new(),
        }
    }
}

impl ConflictPolicy {
    pub fn targets(
        &self,
        evaluation: &trakkin_mappings_language::Evaluation,
    ) -> std::collections::BTreeMap<(String, String), BTreeSet<String>> {
        use trakkin_mappings_language::Operator;
        let mut output = std::collections::BTreeMap::<_, BTreeSet<_>>::new();
        if evaluation.operator != Operator::Exact {
            return output;
        }
        for alignment in &evaluation.alignments {
            let left = &evaluation.left.members[alignment.left].reference;
            let right = &evaluation.right.members[alignment.right].reference;
            for (origin, target) in [(left, right), (right, left)] {
                let Some((origin_source, _)) = origin.split_once("://") else {
                    continue;
                };
                let Some((target_source, _)) = target.split_once("://") else {
                    continue;
                };
                if self.exclusive_source_pairs.iter().any(|pair| {
                    pair.origin_source == origin_source && pair.target_source == target_source
                }) {
                    output
                        .entry((origin.clone(), target_source.to_owned()))
                        .or_default()
                        .insert(target.clone());
                }
            }
        }
        output
    }

    pub fn parse(text: &str) -> Result<Self> {
        let policy: Self = serde_json::from_str(text)?;
        ensure!(
            policy.format_version == 1,
            "unsupported conflict policy version"
        );
        let mut seen = BTreeSet::new();
        for pair in &policy.exclusive_source_pairs {
            for source in [&pair.origin_source, &pair.target_source] {
                ensure!(!source.contains("://"), "invalid source namespace");
                trakkin_mappings_language::parse_reference(&format!("{source}://unit"))?;
            }
            ensure!(
                seen.insert(pair.clone()),
                "duplicate exclusive source-pair policy"
            );
        }
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_validation_does_not_require_catalog_storage() {
        assert!(ConflictPolicy::parse(r#"{"format_version":1,"exclusive_source_pairs":[{"origin_source":"co.anilist","target_source":"org.themoviedb"}]}"#).is_ok());
        assert!(
            ConflictPolicy::parse(r#"{"format_version":2,"exclusive_source_pairs":[]}"#).is_err()
        );
        assert!(ConflictPolicy::parse(r#"{"format_version":1,"exclusive_source_pairs":[{"origin_source":"a://injected","target_source":"b"}]}"#).is_err());
    }
}
