use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::{Change, Operation, Page, storage::Index};

pub(super) fn select_domains(supported: &[&str], selected: &[&str]) -> Result<BTreeSet<String>> {
    ensure!(!selected.is_empty(), "Dataset selection is empty");
    for domain in selected {
        ensure!(
            supported.contains(domain),
            "Unsupported acquisition domain {domain}"
        );
    }
    Ok(selected.iter().map(|domain| (*domain).to_owned()).collect())
}

#[derive(Default)]
pub(super) struct Reconciliation {
    absent: BTreeSet<String>,
    seen: bool,
    complete: bool,
}

impl Reconciliation {
    pub fn restore(operation: Operation, index: &Index, selected: impl Fn(&str) -> bool) -> Self {
        Self {
            absent: if operation == Operation::Reconcile {
                index
                    .iter()
                    .filter(|(key, entry)| {
                        !entry.deleted
                            && key
                                .split_once(':')
                                .is_some_and(|(domain, _)| selected(domain))
                    })
                    .map(|(key, _)| key.clone())
                    .collect()
            } else {
                BTreeSet::new()
            },
            seen: false,
            complete: false,
        }
    }

    pub fn absences(
        &mut self,
        cursor: &Value,
        change: impl Fn(String) -> Change,
    ) -> Result<Option<Page>> {
        let Some(offset) = cursor.get("absent") else {
            return Ok(None);
        };
        ensure!(
            self.complete,
            "Catalogue absence checks require a complete catalogue"
        );
        let offset = offset
            .as_u64()
            .context("Invalid catalogue absence cursor")?;
        let next = offset
            .checked_add(1)
            .context("Catalogue absence cursor overflow")?;
        let keys: Vec<_> = self.absent.iter().take(1000).cloned().collect();
        for key in &keys {
            self.absent.remove(key);
        }
        Ok(Some(Page {
            changes: keys.into_iter().map(change).collect(),
            next: (!self.absent.is_empty()).then(|| json!({"absent":next})),
        }))
    }

    pub fn observe(&mut self, page: &mut Page) -> Result<()> {
        for change in &page.changes {
            self.seen |= matches!(change, Change::Dirty(_));
            self.absent.remove(change.key());
        }
        if page.next.is_none() {
            ensure!(self.seen, "Refusing to reconcile an empty catalogue");
            self.complete = true;
            page.next = (!self.absent.is_empty()).then(|| json!({"absent":0}));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Entry;

    #[test]
    fn selection_is_nonempty_supported_and_deduplicated() {
        assert!(select_domains(&["media"], &[]).is_err());
        assert!(select_domains(&["media"], &["movie"]).is_err());
        assert_eq!(
            select_domains(&["media"], &["media", "media"]).unwrap(),
            BTreeSet::from(["media".into()])
        );
    }

    #[test]
    fn absences_require_a_completed_nonempty_catalogue() {
        let index = Index::from([(
            "media:1".into(),
            Entry {
                hash: "live".into(),
                deleted: false,
                metadata: json!({}),
            },
        )]);
        let mut reconciliation = Reconciliation::restore(Operation::Reconcile, &index, |_| true);
        assert!(
            reconciliation
                .absences(&json!({"absent":0}), Change::Dirty)
                .is_err()
        );
        assert!(
            reconciliation
                .observe(&mut Page {
                    changes: vec![],
                    next: None
                })
                .is_err()
        );
        let mut page = Page {
            changes: vec![Change::Dirty("media:2".into())],
            next: None,
        };
        reconciliation.observe(&mut page).unwrap();
        let absent = reconciliation
            .absences(&page.next.unwrap(), Change::Dirty)
            .unwrap()
            .unwrap();
        assert_eq!(absent.changes[0].key(), "media:1");
        assert!(absent.next.is_none());
    }
}
