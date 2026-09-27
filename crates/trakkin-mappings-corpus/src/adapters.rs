use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};
use trakkin_mappings_language::{Resolved, Resolver, Selection, Selector};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adapters {
    pub version: u32,
    pub sources: BTreeMap<String, Source>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub dimensions: Vec<String>,
    #[serde(default)]
    pub exclusive_with: Vec<String>,
    #[serde(default)]
    pub selections: BTreeMap<String, Resolved>,
}

impl Adapters {
    pub fn load(path: &Path) -> Result<Self> {
        let adapters: Self = serde_json::from_slice(
            &fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        )?;
        ensure!(
            adapters.version == 1,
            "unsupported adapter evidence version"
        );
        for (namespace, source) in &adapters.sources {
            for target in &source.exclusive_with {
                ensure!(
                    adapters.sources.contains_key(target),
                    "unknown exclusive target {target}"
                );
            }
            for (text, resolved) in &source.selections {
                let probe = trakkin_mappings_language::parse(&format!("{text} <~> {text}"))?;
                let trakkin_mappings_language::Expression::Selection(selection) =
                    &probe[0].statement.left
                else {
                    bail!("adapter evidence must name a selection")
                };
                ensure!(
                    selection.source() == namespace && selection.canonical() == *text,
                    "noncanonical or wrong-source evidence key {text}"
                );
                ensure!(
                    resolved
                        .items
                        .iter()
                        .all(|item| item.starts_with(&format!("{namespace}://"))),
                    "resolved units must be source-qualified"
                );
                trakkin_mappings_language::validate(&probe[0].statement, &adapters)?;
            }
        }
        Ok(adapters)
    }

    pub fn fingerprint(&self) -> String {
        trakkin_mappings_language::digest(&[
            b"trakkin:adapters:v1\0",
            &serde_json::to_vec(self).unwrap(),
        ])
    }

    pub fn exclusive(&self, source: &str, target: &str) -> bool {
        self.sources.get(source).is_some_and(|adapter| {
            adapter
                .exclusive_with
                .iter()
                .any(|namespace| namespace == target)
        })
    }
}

fn positive_integer(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('0') && value.bytes().all(|byte| byte.is_ascii_digit())
}

impl Resolver for Adapters {
    fn resolve(&self, selection: &Selection) -> Result<Resolved> {
        let adapter = self
            .sources
            .get(selection.source())
            .with_context(|| format!("no adapter configured for {}", selection.source()))?;
        let opaque = selection.reference.split_once("://").unwrap().1;
        let valid = match selection.source() {
            "com.imdb" => opaque.strip_prefix("title/tt").is_some_and(|value| {
                value.len() >= 7 && value.bytes().all(|byte| byte.is_ascii_digit())
            }),
            "org.themoviedb" => ["movie/", "tv/"]
                .iter()
                .any(|prefix| opaque.strip_prefix(prefix).is_some_and(positive_integer)),
            "com.thetvdb" => ["series/", "movies/", "episodes/"]
                .iter()
                .any(|prefix| opaque.strip_prefix(prefix).is_some_and(positive_integer)),
            "co.anilist" | "net.myanimelist" | "net.anidb" => {
                opaque.strip_prefix("anime/").is_some_and(positive_integer)
            }
            _ => !opaque.is_empty(),
        };
        ensure!(valid, "adapter rejects reference {}", selection.reference);
        if let Some(Selector::Predicates(predicates)) = &selection.selector {
            for dimension in predicates.keys() {
                ensure!(
                    adapter.dimensions.contains(dimension),
                    "{} does not support dimension {dimension}",
                    selection.source()
                );
            }
        }
        if selection.selector.is_none() {
            return Ok(Resolved {
                items: vec![selection.reference.clone()],
                ordered: true,
                coordinates: None,
            });
        }
        adapter.selections.get(&selection.canonical()).cloned().with_context(|| format!("missing offline adapter evidence for {}; add a verified resolution to mappings/v1/adapters.json", selection.canonical()))
    }
}
