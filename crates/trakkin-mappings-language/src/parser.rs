use crate::{Expression, Operator, Record, Selection, Selector, Statement, Value};
use antlr4_runtime::{CommonTokenStream, InputStream};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::num::NonZeroU64;

mod generated_lexer {
    include!(concat!(env!("OUT_DIR"), "/trakkin_lexer.rs"));
}

mod generated_parser {
    include!(concat!(env!("OUT_DIR"), "/trakkin_parser.rs"));
}

use generated_lexer::TrakkinLexer;
use generated_parser::{
    DocumentContext, ExpressionContext, MappingRecordContext, ScalarContext, SelectionContext,
    SelectorContext, SelectorValueContext, StatementContext, ValidatedTreeContext,
};

pub fn parse(text: &str) -> Result<Vec<Record>> {
    let mut lexer = TrakkinLexer::new(InputStream::new(text));
    lexer.remove_error_listeners();
    let mut parser = generated_parser::TrakkinParser::new(CommonTokenStream::new(lexer));
    parser.remove_error_listeners();
    let result = parser.document().context("invalid mapping document")?;
    let tree = generated_parser::TrakkinParserParseOutput { result, parser }
        .validate()
        .context("invalid mapping document")?;
    let document = tree
        .tree()
        .downcast_ref::<DocumentContext<ValidatedTreeContext>>()
        .context("mapping parser returned an unexpected root")?;
    document.mapping_record_children().map(record).collect()
}

fn record(context: MappingRecordContext<'_, ValidatedTreeContext>) -> Result<Record> {
    let metadata = context
        .metadata_line_children()
        .map(|line| {
            line.annotation_token()
                .or_else(|| line.comment_token())
                .context("metadata line has no value")
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
    let left = expression(
        expressions
            .next()
            .context("statement has no left expression")?,
        0,
    )?;
    let right = expression(
        expressions
            .next()
            .context("statement has no right expression")?,
        0,
    )?;
    ensure!(
        expressions.next().is_none(),
        "statement has too many expressions"
    );
    let mapping_operator = context.mapping_operator();
    let operator = if mapping_operator.exact_equivalence_token().is_some() {
        Operator::Exact
    } else if mapping_operator.coverage_equivalence_token().is_some() {
        Operator::Coverage
    } else if mapping_operator.implication_token().is_some() {
        Operator::Implication
    } else {
        bail!("statement has no mapping operator")
    };
    let mut statement = Statement {
        left,
        operator,
        right,
    };
    statement.normalize_extents();
    Ok(statement)
}

fn expression(
    context: ExpressionContext<'_, ValidatedTreeContext>,
    depth: usize,
) -> Result<Expression> {
    ensure!(depth < 128, "composite nesting limit exceeded");
    if let Some(selection) = context.selection() {
        return Ok(Expression::Selection(parse_selection(selection)?));
    }
    let composite = context.composite().context("expression has no value")?;
    Ok(Expression::Composite(
        composite
            .expression_children()
            .map(|child| expression(child, depth + 1))
            .collect::<Result<_>>()?,
    ))
}

fn parse_selection(context: SelectionContext<'_, ValidatedTreeContext>) -> Result<Selection> {
    let selector = context.selector().map(parse_selector).transpose()?;
    let extent = context
        .extent_token()
        .map(|token| {
            token
                .to_string()
                .strip_prefix('@')
                .context("extent is missing its prefix")?
                .parse::<u64>()
                .context("extent exceeds the supported integer range")
                .and_then(|value| NonZeroU64::new(value).context("extent must be positive"))
        })
        .transpose()?
        .unwrap_or_else(|| NonZeroU64::new(1).unwrap());
    Ok(Selection {
        reference: context.reference_token().to_string(),
        selector,
        extent,
    })
}

fn parse_selector(context: SelectorContext<'_, ValidatedTreeContext>) -> Result<Selector> {
    if context.recursive_selector_token().is_some() {
        return Ok(Selector::Recursive);
    }
    let mut predicates = BTreeMap::new();
    for predicate in context.predicate_children() {
        let dimension = predicate.identifier_token().to_string();
        ensure!(
            predicates
                .insert(
                    dimension.clone(),
                    selector_value(predicate.selector_value())?
                )
                .is_none(),
            "duplicate selector dimension {dimension}"
        );
    }
    Ok(Selector::Predicates(predicates))
}

fn selector_value(context: SelectorValueContext<'_, ValidatedTreeContext>) -> Result<Value> {
    if context.wildcard_token().is_some() {
        return Ok(Value::Wildcard);
    }
    if let Some(context) = context.set_value() {
        let mut values = context
            .scalar_children()
            .map(parse_scalar)
            .collect::<Result<Vec<_>>>()?;
        values.sort();
        values.dedup();
        return Ok(Value::Set(values));
    }
    if let Some(context) = context.range_value() {
        let open_start = context.text().starts_with("..");
        let mut values = context.scalar_children().map(parse_scalar);
        let first = values.next().transpose()?.context("range has no bound")?;
        let second = values.next().transpose()?;
        ensure!(values.next().is_none(), "range has too many bounds");
        return if open_start {
            Ok(Value::Range(None, Some(first)))
        } else {
            Ok(Value::Range(Some(first), second))
        };
    }
    Ok(Value::Scalar(parse_scalar(
        context.scalar().context("selector value is empty")?,
    )?))
}

fn parse_scalar(context: ScalarContext<'_, ValidatedTreeContext>) -> Result<String> {
    if let Some(token) = context.quoted_scalar_token() {
        return decode_quoted_scalar(&token.to_string());
    }
    context
        .positive_integer_token()
        .or_else(|| context.identifier_token())
        .or_else(|| context.bare_scalar_token())
        .context("scalar has no token")
        .map(|token| token.to_string())
}

fn decode_quoted_scalar(text: &str) -> Result<String> {
    let inner = text
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .context("invalid quoted scalar")?;
    let mut characters = inner.chars();
    let mut value = String::new();
    while let Some(character) = characters.next() {
        if character != '\\' {
            value.push(character);
            continue;
        }
        value.push(
            match characters.next().context("incomplete scalar escape")? {
                '"' => '"',
                '\\' => '\\',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                _ => bail!("unsupported scalar escape"),
            },
        );
    }
    Ok(value)
}
