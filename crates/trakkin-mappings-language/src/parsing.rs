use crate::{Expression, Operator, Record, Selection, Selector, Statement, Value};
use antlr4_runtime::{CommonTokenStream, InputStream};
use anyhow::{Context, Result, bail, ensure};
use std::{collections::BTreeMap, num::NonZeroU64};

mod lexer {
    include!(concat!(env!("OUT_DIR"), "/trakkin_lexer.rs"));
}

mod parser {
    include!(concat!(env!("OUT_DIR"), "/trakkin_parser.rs"));
}

use parser::{
    DocumentContext, ExpressionContext, MappingRecordContext, ReferenceContext, ScalarContext,
    SelectorContext, SelectorValueContext, StatementContext, ValidatedTreeContext,
};

pub fn parse_reference(text: &str) -> Result<String> {
    let mut lexer = lexer::TrakkinLexer::new(InputStream::new(text));
    lexer.remove_error_listeners();
    let mut parser = parser::TrakkinParser::new(CommonTokenStream::new(lexer));
    parser.remove_error_listeners();
    let result = parser.reference().context("invalid source reference")?;
    let tree = parser::TrakkinParserParseOutput { result, parser }
        .validate()
        .context("invalid source reference")?;
    let reference = tree
        .tree()
        .downcast_ref::<ReferenceContext<ValidatedTreeContext>>()
        .context("unexpected reference parser root")?;
    Ok(reference.reference_token().to_string())
}

pub fn parse(text: &str) -> Result<Vec<Record>> {
    let mut lexer = lexer::TrakkinLexer::new(InputStream::new(text));
    lexer.remove_error_listeners();
    let mut parser = parser::TrakkinParser::new(CommonTokenStream::new(lexer));
    parser.remove_error_listeners();
    let result = parser.document().context("invalid mapping document")?;
    let tree = parser::TrakkinParserParseOutput { result, parser }
        .validate()
        .context("invalid mapping document")?;
    let document = tree
        .tree()
        .downcast_ref::<DocumentContext<ValidatedTreeContext>>()
        .context("unexpected parser root")?;
    document.mapping_record_children().map(record).collect()
}

fn record(context: MappingRecordContext<'_, ValidatedTreeContext>) -> Result<Record> {
    let metadata = context
        .metadata_line_children()
        .map(|line| {
            line.annotation_token()
                .or_else(|| line.comment_token())
                .context("missing metadata")
                .map(|token| token.to_string())
        })
        .collect::<Result<_>>()?;
    Ok(Record {
        metadata,
        statement: statement(context.statement())?,
    })
}

fn statement(context: StatementContext<'_, ValidatedTreeContext>) -> Result<Statement> {
    let mut expressions = context.expression_children();
    let left = expression(expressions.next().context("missing left side")?, 0)?;
    let right = expression(expressions.next().context("missing right side")?, 0)?;
    let operator = context.mapping_operator();
    let operator = if operator.exact_equivalence_token().is_some() {
        Operator::Exact
    } else if operator.coverage_equivalence_token().is_some() {
        Operator::Coverage
    } else {
        Operator::Implication
    };
    let mut statement = Statement {
        left,
        operator,
        right,
    };
    statement.normalize();
    Ok(statement)
}

fn expression(
    context: ExpressionContext<'_, ValidatedTreeContext>,
    depth: usize,
) -> Result<Expression> {
    ensure!(depth < 128, "composite nesting limit exceeded");
    if let Some(selection) = context.selection() {
        let extent = selection
            .extent_token()
            .map(|token| {
                token.to_string()[1..]
                    .parse::<u64>()
                    .context("extent exceeds the supported integer range")
                    .and_then(|value| NonZeroU64::new(value).context("extent must be positive"))
            })
            .transpose()?
            .unwrap_or(NonZeroU64::MIN);
        return Ok(Expression::Selection(Selection {
            reference: selection.reference_token().to_string(),
            selector: selection.selector().map(selector).transpose()?,
            extent,
        }));
    }
    Ok(Expression::Composite(
        context
            .composite()
            .context("missing expression")?
            .expression_children()
            .map(|child| expression(child, depth + 1))
            .collect::<Result<_>>()?,
    ))
}

fn selector(context: SelectorContext<'_, ValidatedTreeContext>) -> Result<Selector> {
    if context.recursive_selector_token().is_some() {
        return Ok(Selector::Recursive);
    }
    let mut predicates = BTreeMap::new();
    for predicate in context.predicate_children() {
        let dimension = predicate.identifier_token().to_string();
        ensure!(
            predicates
                .insert(dimension.clone(), value(predicate.selector_value())?)
                .is_none(),
            "duplicate selector dimension {dimension}"
        );
    }
    Ok(Selector::Predicates(predicates))
}

fn value(context: SelectorValueContext<'_, ValidatedTreeContext>) -> Result<Value> {
    if context.wildcard_token().is_some() {
        return Ok(Value::Wildcard);
    }
    if let Some(set) = context.set_value() {
        let mut values = set
            .scalar_children()
            .map(scalar)
            .collect::<Result<Vec<_>>>()?;
        values.sort();
        values.dedup();
        return Ok(Value::Set(values));
    }
    if let Some(range) = context.range_value() {
        let mut bounds = range.scalar_children().map(scalar);
        let first = bounds.next().transpose()?.context("missing range bound")?;
        return if range.text().starts_with("..") {
            Ok(Value::Range(None, Some(first)))
        } else {
            Ok(Value::Range(Some(first), bounds.next().transpose()?))
        };
    }
    Ok(Value::Scalar(scalar(
        context.scalar().context("missing scalar")?,
    )?))
}

fn scalar(context: ScalarContext<'_, ValidatedTreeContext>) -> Result<String> {
    let Some(quoted) = context.quoted_scalar_token() else {
        return context
            .positive_integer_token()
            .or_else(|| context.identifier_token())
            .or_else(|| context.bare_scalar_token())
            .context("missing scalar token")
            .map(|token| token.to_string());
    };
    let text = quoted.to_string();
    let mut characters = text[1..text.len() - 1].chars();
    let mut decoded = String::new();
    while let Some(character) = characters.next() {
        decoded.push(if character == '\\' {
            match characters.next().context("incomplete scalar escape")? {
                '"' => '"',
                '\\' => '\\',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                _ => bail!("unsupported scalar escape"),
            }
        } else {
            character
        });
    }
    Ok(decoded)
}
