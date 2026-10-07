use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

use super::{Batch, Index, Storage};
use crate::Operation;

pub struct RoutedStorage<S> {
    coordinator: S,
    datasets: BTreeMap<String, S>,
}

impl<S: Storage> RoutedStorage<S> {
    pub fn new(coordinator: S, datasets: BTreeMap<String, S>) -> Result<Self> {
        ensure!(!datasets.is_empty(), "Acquisition job has no datasets");
        for domain in datasets.keys() {
            crate::dataset::validate_component(domain)?;
        }
        Ok(Self {
            coordinator,
            datasets,
        })
    }

    pub fn datasets_mut(&mut self) -> &mut BTreeMap<String, S> {
        &mut self.datasets
    }
}

impl<S: Storage> Storage for RoutedStorage<S> {
    fn index(&mut self) -> Result<Index> {
        let mut index = Index::new();
        for (domain, storage) in &mut self.datasets {
            for (key, entry) in storage.index()? {
                ensure!(
                    !key.is_empty() && !key.starts_with('@'),
                    "Invalid native record ID"
                );
                index.insert(format!("{domain}:{key}"), entry);
            }
        }
        Ok(index)
    }

    fn checkpoint(&mut self, operation: Operation) -> Result<Value> {
        self.coordinator.checkpoint(operation)
    }

    fn commit(&mut self, operation: Operation, checkpoint: &Value, batch: &Batch) -> Result<()> {
        batch.validate()?;
        let mut routed: BTreeMap<String, Batch> = BTreeMap::new();
        for (key, update) in &batch.updates {
            let (domain, native) = key.split_once(':').context("Unqualified acquisition key")?;
            ensure!(
                self.datasets.contains_key(domain),
                "Unplanned acquisition domain {domain}"
            );
            ensure!(
                !native.is_empty() && !native.starts_with('@'),
                "Invalid native record ID"
            );
            routed
                .entry(domain.into())
                .or_default()
                .updates
                .insert(native.into(), update.clone());
        }
        for (domain, batch) in routed {
            self.datasets
                .get_mut(&domain)
                .unwrap()
                .commit(operation, checkpoint, &batch)?;
        }
        self.coordinator
            .commit(operation, checkpoint, &Batch::default())
    }
}
