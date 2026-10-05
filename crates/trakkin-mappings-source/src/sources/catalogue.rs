use anyhow::{Result, ensure};
use std::collections::BTreeSet;

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
