use crate::{
    Expression, Operator, Record, Selection, Selector, Statement, Value, bare_scalar, identifier,
};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;

pub fn parse(text: &str) -> Result<Vec<Record>> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut metadata = Vec::new();
    let mut records = Vec::new();
    for (index, segment) in text.split_inclusive('\n').enumerate() {
        let line = if let Some(line) = segment.strip_suffix('\n') {
            line.strip_suffix('\r').unwrap_or(line)
        } else {
            segment
        };
        let result = (|| -> Result<()> {
            ensure!(
                !line.is_empty(),
                "blank lines are not part of the document grammar"
            );
            ensure!(!line.contains(['\r', '\n']), "unexpected line ending");
            if let Some(annotation) = line.strip_prefix("#@") {
                let (name, value) = annotation
                    .split_once(' ')
                    .context("annotation needs a name and value")?;
                ensure!(identifier(name) && !value.is_empty(), "invalid annotation");
                metadata.push(line.to_owned());
            } else if line == "#" || line.starts_with("# ") {
                metadata.push(line.to_owned());
            } else {
                let mut parser = Parser { remaining: line };
                let left = parser.expression(0)?;
                parser.space()?;
                let operator = if parser.take("<=>") {
                    Operator::Exact
                } else if parser.take("<~>") {
                    Operator::Coverage
                } else if parser.take("=>") {
                    Operator::Implication
                } else {
                    bail!("expected mapping operator")
                };
                parser.space()?;
                let right = parser.expression(0)?;
                ensure!(
                    parser.remaining.is_empty(),
                    "unexpected trailing input: {}",
                    parser.remaining
                );
                records.push(Record {
                    metadata: std::mem::take(&mut metadata),
                    statement: Statement {
                        left,
                        operator,
                        right,
                    },
                });
            }
            Ok(())
        })();
        result.with_context(|| format!("line {}", index + 1))?;
    }
    ensure!(metadata.is_empty(), "metadata must precede a statement");
    Ok(records)
}

struct Parser<'a> {
    remaining: &'a str,
}

impl<'a> Parser<'a> {
    fn take(&mut self, token: &str) -> bool {
        if let Some(rest) = self.remaining.strip_prefix(token) {
            self.remaining = rest;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &str) -> Result<()> {
        ensure!(self.take(token), "expected {token}");
        Ok(())
    }

    fn space(&mut self) -> Result<()> {
        ensure!(self.take(" "), "expected ASCII space");
        self.remaining = self.remaining.trim_start_matches(' ');
        Ok(())
    }

    fn expression(&mut self, depth: usize) -> Result<Expression> {
        ensure!(depth < 128, "composite nesting limit exceeded");
        if self.take("[") {
            let mut expressions = vec![self.expression(depth + 1)?];
            while self.take(",") {
                expressions.push(self.expression(depth + 1)?);
            }
            self.expect("]")?;
            return Ok(Expression::Composite(expressions));
        }
        let boundary = self
            .remaining
            .find(|character: char| {
                character <= ' ' || character == '\u{7f}' || ",<>[]".contains(character)
            })
            .unwrap_or(self.remaining.len());
        let reference = &self.remaining[..boundary];
        let (source, opaque) = reference
            .split_once("://")
            .context("expected source://opaque reference")?;
        ensure!(
            !opaque.is_empty()
                && source.starts_with(|character: char| character.is_ascii_alphabetic())
                && source
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
            "invalid reference"
        );
        self.remaining = &self.remaining[boundary..];
        let selector = if self.remaining.starts_with(' ')
            && self.remaining.trim_start_matches(' ').starts_with(":: ")
        {
            self.space()?;
            self.expect("::")?;
            self.space()?;
            if self.take("**") {
                Some(Selector::Recursive)
            } else {
                let mut predicates = BTreeMap::new();
                loop {
                    let end = self
                        .remaining
                        .find('=')
                        .context("expected dimension=value")?;
                    let dimension = &self.remaining[..end];
                    ensure!(identifier(dimension), "invalid selector dimension");
                    self.remaining = &self.remaining[end + 1..];
                    ensure!(
                        predicates
                            .insert(dimension.to_owned(), self.value()?)
                            .is_none(),
                        "duplicate selector dimension {dimension}"
                    );
                    if let Some(rest) = self.remaining.strip_prefix(',') {
                        let dimension_end = rest
                            .find(|character: char| {
                                !character.is_ascii_alphanumeric()
                                    && character != '_'
                                    && character != '-'
                            })
                            .unwrap_or(rest.len());
                        if rest[dimension_end..].starts_with('=')
                            && identifier(&rest[..dimension_end])
                        {
                            self.remaining = rest;
                            continue;
                        }
                    }
                    break;
                }
                Some(Selector::Predicates(predicates))
            }
        } else {
            None
        };
        Ok(Expression::Selection(Selection {
            reference: reference.to_owned(),
            selector,
        }))
    }

    fn value(&mut self) -> Result<Value> {
        if self.take("{") {
            let mut values = vec![self.scalar()?];
            while self.take(",") {
                values.push(self.scalar()?);
            }
            self.expect("}")?;
            values.sort();
            values.dedup();
            return Ok(Value::Set(values));
        }
        if self.take("*") {
            return Ok(Value::Wildcard);
        }
        if self.take("..") {
            return Ok(Value::Range(None, Some(self.scalar()?)));
        }
        let start = self.scalar()?;
        if self.take("..") {
            let end = if self.remaining.is_empty() || self.remaining.starts_with([',', ']', ' ']) {
                None
            } else {
                Some(self.scalar()?)
            };
            Ok(Value::Range(Some(start), end))
        } else {
            Ok(Value::Scalar(start))
        }
    }

    fn scalar(&mut self) -> Result<String> {
        if self.take("\"") {
            let mut value = String::new();
            loop {
                let character = self
                    .remaining
                    .chars()
                    .next()
                    .context("unterminated quoted scalar")?;
                self.remaining = &self.remaining[character.len_utf8()..];
                match character {
                    '"' => return Ok(value),
                    '\\' => {
                        let escaped = self.remaining.chars().next().context("incomplete escape")?;
                        self.remaining = &self.remaining[escaped.len_utf8()..];
                        value.push(match escaped {
                            '"' => '"',
                            '\\' => '\\',
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            _ => bail!("unsupported escape"),
                        });
                    }
                    character if character < ' ' => bail!("literal control character in scalar"),
                    character => value.push(character),
                }
            }
        }
        let end = self
            .remaining
            .find(|character: char| {
                !character.is_ascii_alphanumeric() && !"_-:+%@.".contains(character)
            })
            .unwrap_or(self.remaining.len());
        let candidate = &self.remaining[..end];
        let end = candidate.find("..").unwrap_or(end);
        let value = &self.remaining[..end];
        ensure!(bare_scalar(value), "invalid scalar");
        self.remaining = &self.remaining[end..];
        Ok(value.to_owned())
    }
}
