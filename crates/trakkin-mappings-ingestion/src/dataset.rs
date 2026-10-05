use crate::{Change, Page, Provider};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;

pub struct AcquisitionPlan {
    provider: Box<dyn Provider>,
    domains: Vec<String>,
}

impl AcquisitionPlan {
    pub fn new(mut provider: Box<dyn Provider>, domains: &[&str]) -> Result<Self> {
        ensure!(!domains.is_empty(), "Acquisition plan has no domains");
        let source = provider
            .source()
            .ok_or_else(|| anyhow::anyhow!("Provider has no source identity"))?;
        for domain in domains {
            select_domains(source, Some(domain))?;
        }
        provider.select_domains(domains);
        Ok(Self {
            provider,
            domains: domains.iter().map(|domain| (*domain).into()).collect(),
        })
    }

    fn includes(&self, key: &str) -> bool {
        key.split_once(':')
            .is_some_and(|(domain, _)| self.domains.iter().any(|selected| selected == domain))
    }

    fn filter(&self, mut page: Page) -> Page {
        page.changes.retain_mut(|change| match change {
            Change::Dirty(key) | Change::Deleted { key, .. } => self.includes(key),
            Change::Membership { keys, .. } => {
                keys.retain(|key| self.includes(key));
                true
            }
        });
        page
    }
}

impl Provider for AcquisitionPlan {
    fn source(&self) -> Option<&'static str> {
        self.provider.source()
    }
    fn restore(&mut self, index: &BTreeMap<String, crate::storage::Entry>) -> Result<()> {
        self.provider.restore(index)
    }
    fn metadata(&self, key: &str) -> Value {
        self.provider.metadata(key)
    }
    fn confirm_absence(&self, key: &str) -> bool {
        self.provider.confirm_absence(key)
    }
    fn enumerate(&mut self, cursor: &Value, started: i64) -> Result<Page> {
        let page = self.provider.enumerate(cursor, started)?;
        Ok(self.filter(page))
    }
    fn discover_changes(&mut self, watermark: i64, cursor: &Value, until: i64) -> Result<Page> {
        let page = self.provider.discover_changes(watermark, cursor, until)?;
        Ok(self.filter(page))
    }
    fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
        self.provider.fetch(key)
    }
    fn fetch_many(&mut self, keys: &[String]) -> Result<BTreeMap<String, Option<Value>>> {
        self.provider.fetch_many(keys)
    }
    fn restart_changes(&self) -> bool {
        self.provider.restart_changes()
    }
}

pub fn job_warehouse(root: &str, source: &str, selected: &[&str]) -> Result<String> {
    ensure!(!selected.is_empty(), "Acquisition plan has no domains");
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

pub fn domains(source: &str) -> Result<&'static [&'static str]> {
    match source {
        "co.anilist" => Ok(&["media"]),
        "org.themoviedb" => Ok(&["movie", "tv", "season", "episode", "episode_group"]),
        "com.thetvdb" => Ok(&["movie", "series", "season", "episode"]),
        _ => anyhow::bail!("Unsupported canonical provider ID"),
    }
}

pub fn warehouse(root: &str, source: &str, domain: &str) -> Result<String> {
    select_domains(source, Some(domain))?;
    ensure!(!root.trim().is_empty(), "Warehouse must not be empty");
    Ok(format!("{}/{source}/{domain}", root.trim_end_matches('/')))
}

pub fn select_domains<'a>(source: &str, domain: Option<&'a str>) -> Result<Vec<&'a str>> {
    let domains = domains(source)?;
    let Some(domain) = domain else {
        return Ok(domains.to_vec());
    };
    ensure!(
        domains.contains(&domain),
        "Unsupported domain {domain} for {source}; choose {}",
        domains.join(", ")
    );
    Ok(vec![domain])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture;
    impl Provider for Fixture {
        fn source(&self) -> Option<&'static str> {
            Some("com.thetvdb")
        }
        fn enumerate(&mut self, _: &Value, _: i64) -> Result<Page> {
            Ok(Page {
                changes: vec![
                    Change::Dirty("movie:123".into()),
                    Change::Dirty("series:123".into()),
                    Change::Deleted {
                        key: "movie:456".into(),
                        metadata: json!({"mergeToId":123}),
                    },
                ],
                next: Some(json!({"page":1})),
            })
        }
        fn discover_changes(&mut self, _: i64, cursor: &Value, until: i64) -> Result<Page> {
            self.enumerate(cursor, until)
        }
        fn fetch(&mut self, key: &str) -> Result<Option<Value>> {
            Ok(Some(json!({"key":key})))
        }
    }

    #[test]
    fn scopes_overlapping_ids_deletions_and_fetches() {
        let mut series = AcquisitionPlan::new(Box::new(Fixture), &["series"]).unwrap();
        let page = series.enumerate(&Value::Null, 0).unwrap();
        assert_eq!(page.changes.len(), 1);
        assert!(matches!(&page.changes[0], Change::Dirty(key) if key == "series:123"));
        assert_eq!(page.next, Some(json!({"page":1})));
        assert_eq!(
            series.fetch("series:123").unwrap().unwrap()["key"],
            "series:123"
        );
        assert_eq!(
            series.fetch_many(&["series:123".into()]).unwrap()["series:123"]
                .as_ref()
                .unwrap()["key"],
            "series:123"
        );
        let mut movies = AcquisitionPlan::new(Box::new(Fixture), &["movie"]).unwrap();
        assert_eq!(
            movies
                .discover_changes(0, &Value::Null, 1)
                .unwrap()
                .changes
                .len(),
            2
        );
        assert_eq!(
            series.fetch("series:native_id.v2").unwrap().unwrap()["key"],
            "series:native_id.v2"
        );
    }

    #[test]
    fn dataset_paths_are_canonical_and_domain_specific() {
        assert_eq!(
            select_domains("com.thetvdb", None).unwrap(),
            vec!["movie", "series", "season", "episode"]
        );
        assert_eq!(
            select_domains("com.thetvdb", Some("series")).unwrap(),
            vec!["series"]
        );
        assert_eq!(
            warehouse("s3://bucket/root/", "com.thetvdb", "series").unwrap(),
            "s3://bucket/root/com.thetvdb/series"
        );
        assert_ne!(
            warehouse("/tmp/mirror", "com.thetvdb", "series").unwrap(),
            warehouse("/tmp/mirror", "com.thetvdb", "movie").unwrap()
        );
        assert!(warehouse("/tmp/mirror", "com.thetvdb", "../series").is_err());
    }
}
