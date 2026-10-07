mod anilist;
mod catalogue;
mod http;
mod tmdb;
mod tvdb;
mod workers;

pub use anilist::AniList;
pub use tmdb::Tmdb;
pub use tvdb::Tvdb;

use anyhow::{Context, Result};
use http::Http;
use serde_json::Value;
use workers::{fetch_many, parallel_map};

fn id(record: &Value, field: &str) -> Result<u64> {
    record[field]
        .as_u64()
        .filter(|id| *id > 0)
        .context("Missing positive upstream record ID")
}

fn array<'a>(record: &'a Value, field: &str) -> Result<&'a Vec<Value>> {
    record[field]
        .as_array()
        .context("Missing upstream catalogue array")
}
