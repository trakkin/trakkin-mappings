mod parser;
mod validation;

pub use parser::parse;
pub use validation::{Resolved, ResolvedExpression, Resolver, validate};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

pub const ID_VERSION: &str = "trakkin:statement:v1\0";

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

    pub fn resolution_key(&self) -> String {
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
        let mut text = self.resolution_key();
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
        format!(
            "{} {} {}",
            self.left.canonical_with_extent_divisor(divisor),
            self.operator.text(),
            self.right.canonical_with_extent_divisor(divisor)
        )
    }

    pub fn id(&self) -> String {
        digest(&[ID_VERSION.as_bytes(), self.canonical().as_bytes()])
    }

    pub fn relation_key(&self) -> String {
        let divisor = self.extent_divisor();
        let mut sides = [
            self.left.canonical_with_extent_divisor(divisor),
            self.right.canonical_with_extent_divisor(divisor),
        ];
        if self.operator != Operator::Implication {
            sides.sort();
        }
        digest(&[
            b"trakkin:relation:v1\0",
            format!("{} {} {}", sides[0], self.operator.text(), sides[1]).as_bytes(),
        ])
    }

    pub(crate) fn normalize_extents(&mut self) {
        let divisor = self.extent_divisor();
        self.left.divide_extents(divisor);
        self.right.divide_extents(divisor);
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

    pub fn corpus_form(&self) -> Result<Self> {
        let mut sources = BTreeSet::new();
        let mut single = BTreeMap::new();
        for line in &self.metadata {
            let annotation = line.strip_prefix("#@").ok_or_else(|| {
                anyhow::anyhow!("canonical corpus requires #@note instead of human comments")
            })?;
            let (name, value) = annotation.split_once(' ').unwrap();
            let value = value.trim_matches(' ');
            ensure!(!value.is_empty(), "empty annotation {name}");
            match name {
                "source" => {
                    sources.insert(value.to_owned());
                }
                "reason" | "note" => {
                    ensure!(
                        single.insert(name, value).is_none(),
                        "duplicate #{name} annotation"
                    );
                    if name == "reason" {
                        ensure!(identifier(value), "reason must be an identifier");
                    }
                }
                _ => bail!("unsupported corpus annotation: {name}"),
            }
        }
        let mut metadata: Vec<_> = sources
            .into_iter()
            .map(|value| format!("#@source {value}"))
            .collect();
        for name in ["reason", "note"] {
            if let Some(value) = single.get(name) {
                metadata.push(format!("#@{name} {value}"));
            }
        }
        Ok(Self {
            metadata,
            statement: self.statement.clone(),
        })
    }
}

pub fn digest(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

pub fn identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn bare_scalar(text: &str) -> bool {
    !text.is_empty()
        && text != "::"
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
