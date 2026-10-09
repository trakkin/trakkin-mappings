#[cfg(feature = "pipeline")]
pub mod adapters;
pub mod git;
#[cfg(feature = "pipeline")]
pub mod pipeline;
mod policy;
#[cfg(feature = "pipeline")]
mod projection;
mod publication;

pub use policy::{ConflictPolicy, ExclusiveSourcePair, POLICY};
pub use publication::{Artifact, Bindings, Resolution, claim_path};
pub use trakkin_mappings_language::{Record, parse_record, parse_reference};
