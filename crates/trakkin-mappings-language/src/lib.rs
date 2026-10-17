mod correspondence;
mod documents;
mod material;
mod parsing;
mod semantics;

pub use correspondence::{
    Correspondence, CorrespondenceCapture, CorrespondenceError, CorrespondenceLimits,
    CorrespondenceRelation, CorrespondenceTail, MaterialAlignment, RationalPosition, UnitPair,
    UnitSpan, compile_correspondence,
};
pub use documents::{Decision, Decisions};
pub use material::{MaterialGraph, MaterialGraphError, MaterialRule, build_material_graph};
pub use parsing::{parse, parse_reference};
pub use semantics::{
    Alignment, Evaluation, Member, ResolvedExpression, ResolvedSelection, Resolver, SemanticError,
    SemanticErrorKind, evaluate,
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::OnceLock;

pub const CLAIM_ID_VERSION: &str = "trakkin:claim:v1\0";
pub const SEMANTIC_VERSION: u32 = 1;

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn semantic_revision() -> String {
    static REVISION: OnceLock<String> = OnceLock::new();
    REVISION
        .get_or_init(|| {
            sha256(
                &[
                    include_bytes!("lib.rs").as_slice(),
                    include_bytes!("correspondence.rs").as_slice(),
                    include_bytes!("documents.rs").as_slice(),
                    include_bytes!("material.rs").as_slice(),
                    include_bytes!("parsing.rs").as_slice(),
                    include_bytes!("semantics.rs").as_slice(),
                    include_bytes!("../grammar/Trakkin.g4").as_slice(),
                    include_bytes!("../Cargo.toml").as_slice(),
                ]
                .concat(),
            )
        })
        .clone()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClaimId(String);

impl ClaimId {
    pub fn hash(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClaimId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "v1:{}", self.0)
    }
}

impl FromStr for ClaimId {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let hash = text.strip_prefix("v1:").unwrap_or_default();
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "expected v1:<64 lowercase SHA-256 hexadecimal characters>"
        );
        Ok(Self(hash.to_owned()))
    }
}

pub fn parse_record(text: &str) -> Result<Record> {
    let mut records = parse(text)?;
    ensure!(records.len() == 1, "expected exactly one mapping claim");
    Ok(records.remove(0))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Scalar(String),
    Range(Option<String>, Option<String>),
    Set(Vec<String>),
    Wildcard,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Selector {
    Recursive,
    Predicates(BTreeMap<String, Value>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub reference: String,
    pub selector: Option<Selector>,
    #[serde(default = "default_extent")]
    pub extent: NonZeroU64,
}

impl Selection {
    pub fn source(&self) -> &str {
        self.reference.split_once("://").unwrap().0
    }

    pub fn canonical(&self) -> String {
        self.canonical_with_extent_divisor(1)
    }

    pub fn selection_key(&self) -> String {
        let mut text = self.reference.clone();
        if let Some(selector) = &self.selector {
            text.push_str(" :: ");
            match selector {
                Selector::Recursive => text.push_str("**"),
                Selector::Predicates(predicates) => text.push_str(
                    &predicates
                        .iter()
                        .map(|(dimension, value)| format!("{dimension}={}", value.canonical()))
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            }
        }
        text
    }

    fn canonical_with_extent_divisor(&self, divisor: u64) -> String {
        let mut text = self.selection_key();
        let extent = self.extent.get() / divisor;
        if extent != 1 {
            text.push_str(&format!(" @{extent}"));
        }
        text
    }
}

fn default_extent() -> NonZeroU64 {
    NonZeroU64::new(1).unwrap()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Expression {
    Selection(Selection),
    Composite(Vec<Expression>),
}

impl Expression {
    pub fn canonical(&self) -> String {
        self.canonical_with_extent_divisor(1)
    }

    fn canonical_with_extent_divisor(&self, divisor: u64) -> String {
        match self {
            Self::Selection(selection) => selection.canonical_with_extent_divisor(divisor),
            Self::Composite(expressions) => format!(
                "[{}]",
                expressions
                    .iter()
                    .map(|expression| expression.canonical_with_extent_divisor(divisor))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    pub fn selections(&self) -> Vec<&Selection> {
        match self {
            Self::Selection(selection) => vec![selection],
            Self::Composite(expressions) => expressions.iter().flat_map(Self::selections).collect(),
        }
    }

    fn divide_extents(&mut self, divisor: u64) {
        match self {
            Self::Selection(selection) => {
                selection.extent = NonZeroU64::new(selection.extent.get() / divisor).unwrap();
            }
            Self::Composite(expressions) => {
                for expression in expressions {
                    expression.divide_extents(divisor);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operator {
    Exact,
    Coverage,
    Implication,
}

impl Operator {
    pub fn text(self) -> &'static str {
        match self {
            Self::Exact => "<=>",
            Self::Coverage => "<~>",
            Self::Implication => "=>",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    pub left: Expression,
    pub operator: Operator,
    pub right: Expression,
}

impl Statement {
    pub fn canonical(&self) -> String {
        let divisor = self.extent_divisor();
        let mut sides = [
            self.left.canonical_with_extent_divisor(divisor),
            self.right.canonical_with_extent_divisor(divisor),
        ];
        if self.operator != Operator::Implication {
            sides.sort();
        }
        format!("{} {} {}", sides[0], self.operator.text(), sides[1])
    }

    pub fn claim_id(&self) -> ClaimId {
        let mut hasher = Sha256::new();
        hasher.update(CLAIM_ID_VERSION.as_bytes());
        hasher.update(self.canonical().as_bytes());
        ClaimId(
            hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub(crate) fn normalize(&mut self) {
        let divisor = self.extent_divisor();
        self.left.divide_extents(divisor);
        self.right.divide_extents(divisor);
        if self.operator != Operator::Implication && self.left.canonical() > self.right.canonical()
        {
            std::mem::swap(&mut self.left, &mut self.right);
        }
    }

    fn extent_divisor(&self) -> u64 {
        self.left
            .selections()
            .into_iter()
            .chain(self.right.selections())
            .map(|selection| selection.extent.get())
            .reduce(greatest_common_divisor)
            .unwrap_or(1)
    }
}

fn greatest_common_divisor(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub metadata: Vec<String>,
    pub statement: Statement,
}

impl Record {
    pub fn canonical(&self) -> String {
        let mut lines = self.metadata.clone();
        lines.push(self.statement.canonical());
        lines.join("\n") + "\n"
    }
}

fn bare_scalar(text: &str) -> bool {
    !text.is_empty()
        && text != "::"
        && !text.strip_prefix('@').is_some_and(|digits| {
            digits
                .bytes()
                .next()
                .is_some_and(|byte| matches!(byte, b'1'..=b'9'))
                && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
        && text.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-:+%@".contains(&byte))
        })
}

fn scalar(text: &str) -> String {
    if bare_scalar(text) {
        return text.to_owned();
    }
    let mut escaped = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

impl Value {
    fn canonical(&self) -> String {
        match self {
            Self::Scalar(value) => scalar(value),
            Self::Range(start, end) => format!(
                "{}..{}",
                start.as_deref().map(scalar).unwrap_or_default(),
                end.as_deref().map(scalar).unwrap_or_default()
            ),
            Self::Set(values) => format!(
                "{{{}}}",
                values
                    .iter()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|value| scalar(value))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Wildcard => "*".to_owned(),
        }
    }
}
