mod anilist;
mod catalog;
mod common;
mod tmdb;
mod tvdb;

pub use anilist::AniList;
pub use catalog::{Catalog, Entity, Unit};
pub use tmdb::Tmdb;
pub use tvdb::Tvdb;

use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct Observation {
    pub field: String,
    pub value: Value,
}

pub struct EpisodeOrder {
    pub reference: String,
    pub context: BTreeMap<String, String>,
    pub observations: Vec<(String, Observation)>,
    pub episodes: Vec<Episode>,
}

pub struct Episode {
    pub reference: String,
    pub season: u64,
    pub episode: u64,
    pub title: Option<String>,
    pub air_date: Option<String>,
    pub linked_reference: Option<String>,
    pub observations: Vec<(String, Observation)>,
}

pub trait SourceAdapter {
    fn prepare(&self, namespace: &str, records: &BTreeMap<String, Value>) -> Result<Catalog>;
}
