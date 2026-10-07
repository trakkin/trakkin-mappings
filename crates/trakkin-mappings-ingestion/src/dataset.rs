use anyhow::{Result, ensure};

pub fn job_warehouse(root: &str, source: &str, selected: &[&str]) -> Result<String> {
    ensure!(!selected.is_empty(), "Dataset selection is empty");
    for domain in selected {
        warehouse(root, source, domain)?;
    }
    let mut selected = selected.to_vec();
    selected.sort_unstable();
    selected.dedup();
    Ok(format!(
        "{}/{source}/_jobs/{}",
        root.trim_end_matches('/'),
        selected.join("+")
    ))
}

pub fn warehouse(root: &str, source: &str, domain: &str) -> Result<String> {
    ensure!(!root.trim().is_empty(), "Warehouse must not be empty");
    for component in [source, domain] {
        validate_component(component)?;
    }
    Ok(format!("{}/{source}/{domain}", root.trim_end_matches('/')))
}

pub(crate) fn validate_component(component: &str) -> Result<()> {
    ensure!(
        !component.is_empty()
            && component != "."
            && component != ".."
            && component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "Invalid dataset path component"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_paths_are_canonical_and_domain_specific() {
        assert_eq!(
            warehouse("s3://bucket/root/", "com.thetvdb", "series").unwrap(),
            "s3://bucket/root/com.thetvdb/series"
        );
        assert_ne!(
            warehouse("/tmp/mirror", "com.thetvdb", "series").unwrap(),
            warehouse("/tmp/mirror", "com.thetvdb", "movie").unwrap()
        );
        assert!(warehouse("/tmp/mirror", "com.thetvdb", "../series").is_err());
        assert!(warehouse("/tmp/mirror", "../provider", "series").is_err());
        assert_eq!(
            warehouse("/tmp/mirror", "example.provider", "custom").unwrap(),
            "/tmp/mirror/example.provider/custom"
        );
        assert_eq!(
            job_warehouse(
                "/tmp/mirror",
                "example.provider",
                &["series", "movie", "series"]
            )
            .unwrap(),
            "/tmp/mirror/example.provider/_jobs/movie+series"
        );
    }
}
