use std::collections::BTreeMap;

use anyhow::{Result, ensure};

use crate::{ClaimId, Record, parse};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Decision {
    pub record: Record,
    pub rejected: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Decisions {
    records: BTreeMap<ClaimId, Decision>,
}

impl Decisions {
    pub fn parse(accepted: &str, rejected: &str) -> Result<Self> {
        Self::from_records(parse(accepted)?, parse(rejected)?)
    }

    pub fn from_records(accepted: Vec<Record>, rejected: Vec<Record>) -> Result<Self> {
        let mut records = BTreeMap::new();
        for (entries, rejected) in [(accepted, false), (rejected, true)] {
            for record in entries {
                let id = record.statement.claim_id();
                ensure!(
                    records.insert(id, Decision { record, rejected }).is_none(),
                    "documents contain duplicate or contradictory claims"
                );
            }
        }
        Ok(Self { records })
    }

    pub fn documents(&self) -> (String, String) {
        let mut accepted = String::new();
        let mut rejected = String::new();
        for decision in self.records.values() {
            let document = if decision.rejected {
                &mut rejected
            } else {
                &mut accepted
            };
            document.push_str(&decision.record.canonical());
        }
        (accepted, rejected)
    }
}

impl IntoIterator for Decisions {
    type Item = (ClaimId, Decision);
    type IntoIter = std::collections::btree_map::IntoIter<ClaimId, Decision>;

    fn into_iter(self) -> Self::IntoIter {
        self.records.into_iter()
    }
}
