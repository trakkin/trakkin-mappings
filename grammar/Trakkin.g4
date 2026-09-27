grammar Trakkin;

/*
 * Trakkin Mapping DSL
 * 
 * The grammar contains no target-specific actions and is suitable for any ANTLR4 code-generation
 * target.
 */

document:
	EOF
	| mappingRecord (NEWLINE mappingRecord)* NEWLINE? EOF;

mappingRecord: metadataLine* statement;

metadataLine: (COMMENT | ANNOTATION) NEWLINE;

statement: expression HSPACE mappingOperator HSPACE expression;

expression: selection | composite;

// Composites are ordered and non-empty. Nested composites are valid because a composite contains
// expressions rather than selections only.
composite: '[' expression (',' expression)* ']';

// Whitespace around :: is intentional: without it, :: remains part of the opaque reference token.
selection:
	REFERENCE (HSPACE SELECTOR_SEPARATOR HSPACE selector)? (HSPACE EXTENT)?;

selector: RECURSIVE_SELECTOR | predicate (',' predicate)*;

predicate: IDENTIFIER '=' selectorValue;

selectorValue: setValue | WILDCARD | rangeValue | scalar;

// Inclusive range. Either bound may be open, but not both.
rangeValue: scalar RANGE scalar? | RANGE scalar;

// Sets are syntactically non-empty. Their unordered semantics are enforced by the specification and
// semantic validator, not by the parser.
setValue: '{' scalar (',' scalar)* '}';

scalar: QUOTED_SCALAR | IDENTIFIER | POSITIVE_INTEGER | BARE_SCALAR;

mappingOperator:
	EXACT_EQUIVALENCE
	| COVERAGE_EQUIVALENCE
	| IMPLICATION;

// Keep metadata lines atomic. Their payload is deliberately opaque to the mapping-expression
// grammar; consumers may split ANNOTATION after "#@" at the first ASCII-space run to obtain the
// annotation name and value.
ANNOTATION: '#@' IDENT_START IDENT_CONTINUE* ' '+ ~[\r\n]+;

// A human comment is either "#" or begins with "# ". Consequently #@... is not a comment.
COMMENT: '#' (' ' ~[\r\n]*)?;

EXACT_EQUIVALENCE: '<=>';

COVERAGE_EQUIVALENCE: '<~>';

IMPLICATION: '=>';

SELECTOR_SEPARATOR: '::';

RECURSIVE_SELECTOR: '**';

RANGE: '..';

WILDCARD: '*';

EXTENT: '@' [1-9] [0-9]*;

POSITIVE_INTEGER: [1-9] [0-9]*;

// References are atomic so the core language never parses the opaque value. The value ends only at
// a reserved reference boundary. Notably, ':' remains legal inside the opaque value, so
// "example://foo::bar" is one REFERENCE.
REFERENCE: SOURCE_NAME '://' OPAQUE_CHAR+;

QUOTED_SCALAR: '"' (QUOTED_CHAR | ESCAPE_SEQUENCE)* '"';

// Selector dimensions use identifiers. IDENTIFIER is also accepted as a scalar so common values
// such as "director-cut" do not need a second token form.
IDENTIFIER: IDENT_START IDENT_CONTINUE*;

// Bare scalars may contain the conservative character set defined by the spec. A single '.' may
// separate segments; '..' is reserved for RANGE.
BARE_SCALAR: BARE_SEGMENT ('.' BARE_SEGMENT)*;

// Structural whitespace is ASCII space only. Tabs are not accepted as syntax whitespace. One token
// represents one-or-more spaces, so parser rules can require whitespace without prescribing
// canonical width.
HSPACE: ' '+;

NEWLINE: '\r'? '\n';

// Source namespaces are intentionally permissive. Reverse-domain names are a specification
// recommendation, not a syntactic requirement.
fragment SOURCE_NAME: [A-Za-z] [A-Za-z0-9._-]*;

// References reserve ASCII control/space plus ',', '<', '>', '[' and ']'. Other characters belong
// to the source-owned opaque value.
fragment OPAQUE_CHAR:
	~[\u0000-\u0020\u002C\u003C\u003E\u005B\u005D\u007F];

fragment IDENT_START: [A-Za-z_];

fragment IDENT_CONTINUE: [A-Za-z0-9_-];

fragment BARE_SEGMENT: BARE_CHAR+;

fragment BARE_CHAR: [A-Za-z0-9_:+%@-];

// Printable characters are accepted literally except quote and backslash. Control characters must
// not appear literally in quoted scalars.
fragment QUOTED_CHAR: ~["\\\u0000-\u001F];

fragment ESCAPE_SEQUENCE: '\\' ('"' | '\\' | 'n' | 'r' | 't');
